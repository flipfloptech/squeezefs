//! ENOSPC — terminal for a write that would CREATE custody (charter §6.2
//! "Never-lossy writeback"; `docs/porting/data_alloc_reclaim.md` §4).
//! PORTABLE; every wait is a `sync.Notify` deadline park on the allocator's
//! supply edge over `std.Io`.
//!
//! The law: the never-lossy ladder covers ACKED custody; a write that would
//! create custody of a block the set cannot allocate is refused BEFORE the
//! ack. The verdict is each allocator's OWN terminal `StorageFull` latch —
//! an unreachable holder is retryable and never latches; the latch is
//! INERT while any supply is visible; there is deliberately NO time-bounded
//! re-probe that admits a write (an admitted write on a still-full volume
//! is acked custody that can never land). Custody the daemon already holds
//! (a mapping, a parked buffer, a staged copy, an open overlay record)
//! keeps the ladder: a passthrough whole-block rewrite of a mapped block
//! lands in place, space-neutral; a hole below EOF holds nothing and is
//! refused like growth. WHY: on the 1.3.0 chain (generic/751) every
//! post-fill write acked into a parked buffer, staging filled, the parked
//! set grew past the memory cap to a **74 GiB daemon on a 24 GiB volume**,
//! and the unmount lost it all.
//!
//! The ENOSPC valve: the reference re-tried the mint across ≤ 32 passes,
//! each awaiting a FULL off-thread drain of the reclaim queue, because its
//! unbounded predecessor spun while concurrent reclaim traffic kept
//! `pending` true (RES-15). Here the 32 passes collapse to ONE predicate —
//! `reclaim MADE PROGRESS since this allocation's last failed mint`
//! (`finished_total` advanced): a pass that drained to empty and freed
//! nothing proves there is nothing more reclaim can give (terminal); a pass
//! that freed blocks and still failed the mint lost them to racers, which
//! is the system progressing, so it tries again. No count, no spin: every
//! retry consumed a reclaimed block somewhere.
//!
//! The write-path park (`allocateBlockGraceBounded`) is wall-bounded:
//! `pressure_park_wall = max(2 × routine fence bound, 1 s)` — a DURATION
//! derived from the plane's most patient published bound, never the
//! reallocation LABEL (an owner-clock instant whose "nothing owed" sentinel
//! is `u64::MAX` on every non-owner mount: reading it as a wait made the
//! park unbounded on every co-writer — the s11 fleet wedge, 100 writes in
//! flight for 30 min, `.stats` in D-state). Rung 4 has no reader plane:
//! the base bound is 0, the wall exactly 1 s, and the grace ring is empty,
//! so the write path never parks at all — the law is in place for rung 6.

const std = @import("std");
const testkit = @import("../core/testkit.zig");
const Io = std.Io;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const log = @import("../core/log.zig");
const derive = @import("../core/derive.zig");
const block_alloc = @import("block_alloc.zig");
const reclaim = @import("reclaim.zig");

pub const BlockAllocator = block_alloc.BlockAllocator;
pub const AllocError = block_alloc.AllocError;
pub const Minted = block_alloc.Minted;

// ---------------------------------------------------------------------------
// Derivations (FG:L3009-3035).
// ---------------------------------------------------------------------------

/// The wall's floor: one second — past it the refusal stands, loud: the
/// plane is broken, not slow.
pub const PRESSURE_PARK_WALL_FLOOR_MS: u64 = 1000;
/// The slice when the plane publishes no bound: one manners tick.
pub const PRESSURE_PARK_SLICE_COLD_MS: u64 = reclaim.MANNERS_TICK_MS;
/// Slice bounds: 10 ms (a device command's order) to 250 ms (the drain-rate
/// window — a park never outlives one re-learning of the drain).
pub const PRESSURE_PARK_SLICE_FLOOR_MS: u64 = 10;
pub const PRESSURE_PARK_SLICE_CEILING_MS: u64 = 250;
/// A slice is `bound ÷ 16`: sixteen re-checks per wall.
pub const PRESSURE_PARK_SLICES_PER_WALL: u64 = 16;

/// `max(2 × fence_bound_base, 1 s)`.
pub fn pressureParkWallMs(fence_bound_base_ms: u64) u64 {
    return @max(2 * fence_bound_base_ms, PRESSURE_PARK_WALL_FLOOR_MS);
}

/// `bound == 0 ? 50 : clamp(bound ÷ 16, 10, 250)`.
pub fn pressureParkSliceMs(pressure_bound_ms: u64) u64 {
    if (pressure_bound_ms == 0) return PRESSURE_PARK_SLICE_COLD_MS;
    return derive.clamp(pressure_bound_ms / PRESSURE_PARK_SLICES_PER_WALL, PRESSURE_PARK_SLICE_FLOOR_MS, PRESSURE_PARK_SLICE_CEILING_MS);
}

/// The reader plane's routine fence bound (rung 6's free-grace ring
/// publishes it); absent ⇒ 0 ⇒ the wall is exactly 1 s.
pub const FenceBoundSource = struct {
    ctx: ?*anyopaque,
    baseMs: *const fn (?*anyopaque) u64,
};

pub fn fenceBoundBaseMs(src: ?FenceBoundSource) u64 {
    const s = src orelse return 0;
    return s.baseMs(s.ctx);
}

// ---------------------------------------------------------------------------
// Stats.
// ---------------------------------------------------------------------------

pub const stats = struct {
    pub var write_fresh_block_enospc_refusals: counters.ShardedCounter = .{};
    pub var alloc_pressure_parks: counters.ShardedCounter = .{};
    pub var alloc_pressure_park_wall_refusals: counters.ShardedCounter = .{};
    pub var alloc_valve_passes: counters.ShardedCounter = .{};
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"write_fresh_block_enospc_refusals\":{d},\"alloc_pressure_parks\":{d},\"alloc_pressure_park_wall_refusals\":{d},\"alloc_valve_passes\":{d}}}", .{ stats.write_fresh_block_enospc_refusals.load(), stats.alloc_pressure_parks.load(), stats.alloc_pressure_park_wall_refusals.load(), stats.alloc_valve_passes.load() });
}

// ---------------------------------------------------------------------------
// The placement-candidate census (RT:L3553-3605).
// ---------------------------------------------------------------------------

/// `fresh_supply_exhausted()` over the placement table's ELIGIBLE rows:
/// `true` iff EVERY candidate's allocator is latched exhausted (an empty
/// candidate set is `false` — the allocation path refuses "no healthy
/// backends" itself). ONE relaxed load of the process count short-circuits
/// the census while no allocator is latched. Publishes the `.stats` face
/// `(latched, candidates)`.
pub fn freshSupplyExhausted(candidates: []const *BlockAllocator) bool {
    if (block_alloc.fresh_supply_latched.load(.monotonic) == 0) {
        block_alloc.stats.alloc_fresh_supply_exhausted_latched.set(0);
        block_alloc.stats.alloc_fresh_supply_exhausted_candidates.set(candidates.len);
        return false;
    }
    var exhausted: u64 = 0;
    for (candidates) |a| {
        if (a.freshSupplyExhausted()) exhausted += 1;
    }
    block_alloc.stats.alloc_fresh_supply_exhausted_latched.set(exhausted);
    block_alloc.stats.alloc_fresh_supply_exhausted_candidates.set(candidates.len);
    return candidates.len != 0 and exhausted == candidates.len;
}

pub const WriteAdmission = enum {
    admit,
    /// `ENOSPC` before any byte is recorded; acked custody is unaffected.
    refuse_enospc,
};

/// The staged write path's decision for the WHOLE write ahead of every
/// block future (FC:L20845-20881).
pub fn admitWrite(set_exhausted: bool, write_would_create_fresh_custody: bool) WriteAdmission {
    if (set_exhausted and write_would_create_fresh_custody) {
        stats.write_fresh_block_enospc_refusals.inc();
        return .refuse_enospc;
    }
    return .admit;
}

// ---------------------------------------------------------------------------
// The valve and the funnel (BA:L2174-2381).
// ---------------------------------------------------------------------------

/// The reclaim queue's face to the allocator: an off-thread drain to empty
/// and the progress counter the ONE predicate reads.
pub const Valve = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        /// Drain the queue to empty (waiting on in-flight entries other
        /// venues own); returns the entries THIS drain processed.
        drain: *const fn (*anyopaque, Io) u64,
        /// Entries finished by any venue since the process started.
        finished: *const fn (*anyopaque) u64,
        /// `!fence_halted ∧ queue non-empty`.
        pending: *const fn (*anyopaque) bool,
    };

    pub fn fromReclaimer(r: *reclaim.Reclaimer) Valve {
        const V = struct {
            fn drain(ctx: *anyopaque, io: Io) u64 {
                const rr: *reclaim.Reclaimer = @ptrCast(@alignCast(ctx));
                return rr.drainSync(io);
            }
            fn finished(ctx: *anyopaque) u64 {
                const rr: *reclaim.Reclaimer = @ptrCast(@alignCast(ctx));
                return rr.finishedTotal();
            }
            fn pending(ctx: *anyopaque) bool {
                const rr: *reclaim.Reclaimer = @ptrCast(@alignCast(ctx));
                return rr.pending();
            }
            const vtable: VTable = .{ .drain = &drain, .finished = &finished, .pending = &pending };
        };
        return .{ .ctx = r, .vtable = &V.vtable };
    }
};

/// Park on the allocator's supply edge until a trim claim window returns
/// its block (bounded by the park ceiling: a window is pending supply,
/// never a deficit — a full store under rewrite can hold its ENTIRE free
/// list inside windows, and refusing then was a spurious TERMINAL ENOSPC,
/// R7). Counted `alloc_trim_window_parks` (≈ 0 except exactly that regime).
fn awaitTrimReturn(alloc: *BlockAllocator, io: Io) void {
    block_alloc.stats.alloc_trim_window_parks.fire("an allocation parked on an open trim claim window");
    const deadline = clock.monotonicNs() + reclaim.PARK_BOUND_CEILING_MS * std.time.ns_per_ms;
    while (alloc.trimClaimed() > 0 and clock.monotonicNs() < deadline) {
        var n = alloc.supply_edge.notified();
        _ = n.enable(io);
        if (alloc.trimClaimed() == 0) {
            n.cancel(io);
            return;
        }
        _ = n.waitUntilUncancelable(io, deadline);
    }
}

/// `allocate_block` through the ENOSPC valve: on `StorageFull`, an open
/// trim window parks (the block returns to the list); else ONE drain pass
/// to empty — retried while that pass observed reclaim PROGRESS (module
/// doc), terminal the moment a pass observed none. Every other error
/// returns as-is.
pub fn allocateWithValve(alloc: *BlockAllocator, io: Io, valve: ?Valve) AllocError!Minted {
    while (true) {
        const err = if (alloc.allocateBlock()) |m| return m else |e| e;
        if (err != error.StorageFull) return err;
        if (alloc.trimClaimed() > 0) {
            awaitTrimReturn(alloc, io);
            continue;
        }
        const v = valve orelse return err;
        stats.alloc_valve_passes.inc();
        const before = v.vtable.finished(v.ctx);
        _ = v.vtable.drain(v.ctx, io);
        const after = v.vtable.finished(v.ctx);
        if (after == before and alloc.trimClaimed() == 0) {
            // THE predicate: the pass freed nothing and nobody else did
            // meanwhile ⇒ reclaim has nothing more to give. One honest last
            // try (a free may have landed outside the queue), then terminal.
            return alloc.allocateBlock();
        }
        // Progress: the retry lands on it, or a racer took it — either way
        // the system moved; try again.
    }
}

/// What a `StorageFull` from the valve means to the caller.
pub const SupplyWait = enum {
    /// Refuse at once: the caller holds a block guard, or has a cheaper
    /// arm to fall to (the overlay store declines to accumulation — the
    /// fresh mint YIELDS to the flush under scarce supply).
    none,
    /// Park on the allocator's supply edge, a slice at a time up to the
    /// wall, while its supply is VISIBLE (`freshSupplyVisible`: a window
    /// a top-up is refilling, blocks inside a trim window, a reclaim drain
    /// pending, the grace ring) — the RAM-freeing flush's form: it has no
    /// cheaper arm, and the cell it holds returns only when it lands.
    /// Supply gone invisible is the terminal verdict at once.
    visible_supply,
};

/// `allocate_block_grace_bounded`: the WRITE PATH's form. `.none` is
/// byte-identical to `allocateWithValve` (one branch). `.visible_supply`:
/// while the allocator's supply is visible, park `slice` at a time on the
/// supply edge (the top-up, the free, the trim return and the drain ring
/// it) up to the WALL, then refuse loud; invisible ⇒ terminal at once.
pub fn allocateBlockGraceBounded(alloc: *BlockAllocator, io: Io, valve: ?Valve, wait: SupplyWait, fence_bound: ?FenceBoundSource) AllocError!Minted {
    const started = clock.monotonicNs();
    while (true) {
        const err = if (allocateWithValve(alloc, io, valve)) |m| return m else |e| e;
        if (err != error.StorageFull or wait == .none or !alloc.freshSupplyVisible()) return err;
        const base = fenceBoundBaseMs(fence_bound);
        const wall_ns = pressureParkWallMs(base) * std.time.ns_per_ms;
        const waited = clock.monotonicNs() - started;
        if (waited >= wall_ns) {
            stats.alloc_pressure_park_wall_refusals.inc();
            log.err("data volume {d}: StorageFull after parking {d} ms for visible supply ({d} block(s) in sight; wall {d} ms = max(2 × fence bound {d} ms, 1 s)) — refusing; the plane is broken, not slow", .{ alloc.cfg.vol, waited / std.time.ns_per_ms, alloc.freeSupplyBlocks(), wall_ns / std.time.ns_per_ms, base });
            return err;
        }
        stats.alloc_pressure_parks.inc();
        const slice_ns = pressureParkSliceMs(base) * std.time.ns_per_ms;
        var n = alloc.supply_edge.notified();
        _ = n.enable(io);
        _ = n.waitUntilUncancelable(io, @min(started + wall_ns, clock.monotonicNs() + slice_ns));
    }
}

/// What a placement is FOR: the file and the block index, so the picker can
/// keep a file's stripe unit on one volume (`router.stripeUnitChoice`).
pub const Place = struct { ino: u64, block: u32 };

/// The placement pick (`write_admit.Picker` binds the volumes' allocators
/// behind it).
pub const Picker = struct {
    ctx: ?*anyopaque,
    pick: *const fn (?*anyopaque, Place) ?*BlockAllocator,
};

/// §11 item 13: closes the mount's open rewrite epochs and frees their
/// parked displaced keys; `true` iff it released anything.
pub const PressureSupplyHook = struct {
    ctx: ?*anyopaque,
    run: *const fn (?*anyopaque) bool,
};

pub const PlacedError = AllocError || error{NoHealthyBackends};

/// A placed mint WITH the allocator that minted it (the write path binds
/// the destination to its volume).
pub const PlacedMint = struct { alloc: *BlockAllocator, minted: Minted };

/// `allocate_placed_block`: pick + the grace-bounded mint on the ONE picked
/// volume; a `StorageFull` runs the pressure-supply hook and retries the
/// mint ONCE with a fresh pick. The `under_guard` form (a caller holding a
/// level-3.5 stripe or a 4a guard) skips the hook — the lock law — and
/// never waits (`wait` is the caller's class; a guard holder passes
/// `.none`).
pub fn allocatePlacedMint(picker: Picker, place: Place, io: Io, valve: ?Valve, hook: ?PressureSupplyHook, under_guard: bool, wait: SupplyWait) PlacedError!PlacedMint {
    assert(!under_guard or wait == .none);
    const first = picker.pick(picker.ctx, place) orelse return error.NoHealthyBackends;
    const err = if (allocateBlockGraceBounded(first, io, valve, wait, null)) |m| return .{ .alloc = first, .minted = m } else |e| e;
    if (err != error.StorageFull) return err;
    if (!under_guard) {
        if (hook) |h| _ = h.run(h.ctx);
    }
    const second = picker.pick(picker.ctx, place) orelse return error.NoHealthyBackends;
    return .{ .alloc = second, .minted = try allocateBlockGraceBounded(second, io, valve, wait, null) };
}

pub fn allocatePlaced(picker: Picker, place: Place, io: Io, valve: ?Valve, hook: ?PressureSupplyHook, under_guard: bool, wait: SupplyWait) PlacedError!Minted {
    return (try allocatePlacedMint(picker, place, io, valve, hook, under_guard, wait)).minted;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the_park_wall_is_a_duration_never_the_reallocation_label: max(2 × base, 1 s); slices clamp(bound/16, 10, 250), 50 cold" {
    try testing.expectEqual(@as(u64, 1000), pressureParkWallMs(0)); // rung 4: no reader plane
    try testing.expectEqual(@as(u64, 1000), pressureParkWallMs(400));
    try testing.expectEqual(@as(u64, 1200), pressureParkWallMs(600));
    try testing.expectEqual(@as(u64, 50), pressureParkSliceMs(0));
    try testing.expectEqual(@as(u64, 10), pressureParkSliceMs(16));
    try testing.expectEqual(@as(u64, 62), pressureParkSliceMs(1000));
    try testing.expectEqual(@as(u64, 250), pressureParkSliceMs(100_000));
    try testing.expectEqual(@as(u64, 0), fenceBoundBaseMs(null));
    const Src = struct {
        fn base(_: ?*anyopaque) u64 {
            return 700;
        }
    };
    try testing.expectEqual(@as(u64, 1400), pressureParkWallMs(fenceBoundBaseMs(.{ .ctx = null, .baseMs = &Src.base })));
}

fn mkAlloc(blocks: u64, vol: u16) !*BlockAllocator {
    return BlockAllocator.init(testing.allocator, .{ .vol_tag = vol, .vol = vol, .block_size = 4096, .capacity_bytes = blocks * 4096 });
}

test "fresh_block_writes_against_an_exhausted_set_are_refused_before_the_ack; acked custody keeps the ladder; the latch clears when supply returns" {
    const a = try mkAlloc(2, 0);
    defer a.deinit();
    const b = try mkAlloc(2, 1);
    defer b.deinit();
    const cands = [_]*BlockAllocator{ a, b };
    try testing.expect(!freshSupplyExhausted(&cands));
    _ = try a.allocateBlock();
    _ = try a.allocateBlock();
    try testing.expectError(error.StorageFull, a.allocateBlock());
    // One of two candidates latched: not exhausted (the other can place).
    try testing.expect(!freshSupplyExhausted(&cands));
    _ = try b.allocateBlock();
    _ = try b.allocateBlock();
    try testing.expectError(error.StorageFull, b.allocateBlock());
    try testing.expect(freshSupplyExhausted(&cands));
    try testing.expectEqual(@as(u64, 2), block_alloc.stats.alloc_fresh_supply_exhausted_latched.load());
    try testing.expectEqual(@as(u64, 2), block_alloc.stats.alloc_fresh_supply_exhausted_candidates.load());
    const refusals = stats.write_fresh_block_enospc_refusals.load();
    try testing.expectEqual(WriteAdmission.refuse_enospc, admitWrite(true, true));
    try testing.expectEqual(WriteAdmission.admit, admitWrite(true, false)); // a mapped block's rewrite lands in place
    try testing.expectEqual(WriteAdmission.admit, admitWrite(false, true));
    try testing.expectEqual(refusals + 1, stats.write_fresh_block_enospc_refusals.load());
    // Supply returns on one volume: the set is no longer exhausted.
    _ = try a.freeBlock(0);
    try testing.expect(!freshSupplyExhausted(&cands));
    try testing.expect(!freshSupplyExhausted(&.{})); // an empty candidate set is never "exhausted"
}

const FakeValve = struct {
    alloc: *BlockAllocator,
    /// Blocks the drain frees per pass (0 = nothing left to reclaim).
    frees_per_pass: []const u64,
    pass: usize = 0,
    finished_n: u64 = 0,
    drains: u32 = 0,

    fn drain(ctx: *anyopaque, _: Io) u64 {
        const self: *FakeValve = @ptrCast(@alignCast(ctx));
        self.drains += 1;
        if (self.pass >= self.frees_per_pass.len) return 0;
        const n = self.frees_per_pass[self.pass];
        self.pass += 1;
        var i: u64 = 0;
        while (i < n) : (i += 1) _ = self.alloc.freeBlock(i) catch unreachable;
        self.finished_n += n;
        return n;
    }
    fn finished(ctx: *anyopaque) u64 {
        const self: *FakeValve = @ptrCast(@alignCast(ctx));
        return self.finished_n;
    }
    fn pending(_: *anyopaque) bool {
        return false;
    }
    const vtable: Valve.VTable = .{ .drain = &drain, .finished = &finished, .pending = &pending };
    fn valve(self: *FakeValve) Valve {
        return .{ .ctx = self, .vtable = &FakeValve.vtable };
    }
};

test "enospc_pressure_valve_drains_queued_reclaims_before_refusing; a_full_store_with_nothing_reclaimable_refuses_at_once (the ONE predicate)" {
    const io = testing.io;
    const a = try mkAlloc(3, 0);
    defer a.deinit();
    var i: u64 = 0;
    while (i < 3) : (i += 1) _ = try a.allocateBlock();
    // No valve: terminal at once.
    try testing.expectError(error.StorageFull, allocateWithValve(a, io, null));
    // A valve whose drain frees one block: the retry lands on it.
    var fv = FakeValve{ .alloc = a, .frees_per_pass = &.{1} };
    const passes0 = stats.alloc_valve_passes.load();
    const m = try allocateWithValve(a, io, fv.valve());
    try testing.expectEqual(@as(u64, 0), m.idx);
    try testing.expectEqual(@as(u32, 1), fv.drains);
    try testing.expectEqual(passes0 + 1, stats.alloc_valve_passes.load());
    // Nothing reclaimable: ONE drain pass that frees nothing, one last try, terminal — no 32-pass spin.
    var empty = FakeValve{ .alloc = a, .frees_per_pass = &.{} };
    try testing.expectError(error.StorageFull, allocateWithValve(a, io, empty.valve()));
    try testing.expectEqual(@as(u32, 1), empty.drains);
    // Progress that racers steal: the mint retries while `finished` advances, stops when it stops.
    const Racer = struct {
        alloc: *BlockAllocator,
        frees: []const u64,
        pass: usize = 0,
        finished_n: u64 = 0,
        drains: u32 = 0,
        fn drain(ctx: *anyopaque, _: Io) u64 {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            self.drains += 1;
            if (self.pass >= self.frees.len) return 0;
            self.pass += 1;
            // Free a block and let a racer take it back before our retry.
            _ = self.alloc.freeBlock(1) catch unreachable;
            _ = self.alloc.allocateBlock() catch unreachable;
            self.finished_n += 1;
            return 1;
        }
        fn finished(ctx: *anyopaque) u64 {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            return self.finished_n;
        }
        fn pending(_: *anyopaque) bool {
            return true; // forever "pending" — the RES-15 shape the count existed for
        }
        const vt: Valve.VTable = .{ .drain = &drain, .finished = &finished, .pending = &pending };
    };
    var racer = Racer{ .alloc = a, .frees = &.{ 1, 1, 1 } };
    try testing.expectError(error.StorageFull, allocateWithValve(a, io, .{ .ctx = &racer, .vtable = &Racer.vt }));
    try testing.expectEqual(@as(u32, 4), racer.drains); // three progressing passes + the one that proved exhaustion
}

test "allocation racing a trim claim window parks and lands on the returned block" {
    const io = testing.io;
    const a = try mkAlloc(2, 0);
    defer a.deinit();
    _ = try a.allocateBlock();
    _ = try a.allocateBlock();
    _ = try a.freeBlock(1);
    var g = a.claimFreeForTrim(1).?;
    try testing.expectEqual(@as(u64, 0), a.freeSupplyBlocks());
    const Returner = struct {
        fn run(io_: Io, alloc: *BlockAllocator, guard: *block_alloc.InflightAllocGuard) void {
            while (alloc.supply_edge.waiters(io_) == 0) std.Thread.yield() catch {};
            alloc.returnFromTrim(1, guard);
            alloc.supply_edge.notifyWaiters(io_);
        }
    };
    var fut = io.concurrent(Returner.run, .{ io, a, &g }) catch |err| switch (err) {
        error.ConcurrencyUnavailable => return testkit.skip(@src(), .no_concurrency, "the executor refused a unit of concurrency"),
    };
    const parks = counters.tripwireCount("alloc_trim_window_parks");
    block_alloc.stats.alloc_trim_window_parks.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    const m = try allocateWithValve(a, io, null);
    fut.await(io);
    try log.testing.expectAllFired();
    try testing.expectEqual(@as(u64, 1), m.idx);
    try testing.expectEqual(parks + 1, counters.tripwireCount("alloc_trim_window_parks"));
}

test "the write path's grace-bounded mint: `.none` ⇒ byte-identical to the valve; `.visible_supply` on an exhausted set ⇒ terminal at once; with supply in sight ⇒ parks slices up to the wall then refuses loud; a free during the park lands the mint" {
    const io = testing.io;
    const a = try mkAlloc(1, 0);
    defer a.deinit();
    _ = try a.allocateBlock();
    const parks0 = stats.alloc_pressure_parks.load();
    const wall0 = stats.alloc_pressure_park_wall_refusals.load();
    try testing.expectError(error.StorageFull, allocateBlockGraceBounded(a, io, null, .none, null));
    try testing.expectEqual(parks0, stats.alloc_pressure_parks.load()); // `.none` never parks
    // Nothing in sight: the terminal verdict at once, no park.
    try testing.expectError(error.StorageFull, allocateBlockGraceBounded(a, io, null, .visible_supply, null));
    try testing.expectEqual(parks0, stats.alloc_pressure_parks.load());
    // Supply in sight that never arrives (a reclaim drain "pending" forever) with the floor wall: slices then the wall refusal.
    const Pending = struct {
        fn pending(_: ?*anyopaque) bool {
            return true;
        }
    };
    a.space_pending = .{ .ctx = null, .pending = &Pending.pending };
    const Src = struct {
        fn base(_: ?*anyopaque) u64 {
            return 0;
        }
    };
    const t0 = clock.monotonicNs();
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.StorageFull, allocateBlockGraceBounded(a, io, null, .visible_supply, .{ .ctx = null, .baseMs = &Src.base }));
    try log.testing.expectAllFired();
    const waited = clock.monotonicNs() - t0;
    try testing.expect(waited >= PRESSURE_PARK_WALL_FLOOR_MS * std.time.ns_per_ms);
    try testing.expect(waited < 5 * std.time.ns_per_s);
    try testing.expect(stats.alloc_pressure_parks.load() > parks0);
    try testing.expectEqual(wall0 + 1, stats.alloc_pressure_park_wall_refusals.load());
    // Supply that ARRIVES during the park: the free rings the edge, the mint lands on it.
    const Freer = struct {
        fn run(io_: Io, alloc: *BlockAllocator) void {
            while (alloc.supply_edge.waiters(io_) == 0) std.Thread.yield() catch {};
            _ = alloc.freeBlock(0) catch unreachable;
            alloc.supply_edge.notifyWaiters(io_);
        }
    };
    var fut = io.concurrent(Freer.run, .{ io, a }) catch |err| switch (err) {
        error.ConcurrencyUnavailable => return testkit.skip(@src(), .no_concurrency, "the executor refused a unit of concurrency"),
    };
    const t1 = clock.monotonicNs();
    const m = try allocateBlockGraceBounded(a, io, null, .visible_supply, null);
    fut.await(io);
    try testing.expectEqual(@as(u64, 0), m.idx);
    try testing.expect(clock.monotonicNs() - t1 < PRESSURE_PARK_WALL_FLOOR_MS * std.time.ns_per_ms);
    try testing.expectEqual(wall0 + 1, stats.alloc_pressure_park_wall_refusals.load());
}

test "allocatePlaced: pick + mint; StorageFull runs the pressure hook (not under a guard) and retries once with a fresh pick; no candidates refuses" {
    const io = testing.io;
    const full = try mkAlloc(1, 0);
    defer full.deinit();
    _ = try full.allocateBlock();
    const open = try mkAlloc(4, 1);
    defer open.deinit();
    const P = struct {
        var picks: u32 = 0;
        var hook_runs: u32 = 0;
        var first: ?*BlockAllocator = null;
        var second: ?*BlockAllocator = null;
        fn pick(_: ?*anyopaque, _: Place) ?*BlockAllocator {
            picks += 1;
            return if (picks == 1) first else second;
        }
        fn hook(_: ?*anyopaque) bool {
            hook_runs += 1;
            return false;
        }
    };
    P.first = full;
    P.second = open;
    const m = try allocatePlaced(.{ .ctx = null, .pick = &P.pick }, .{ .ino = 1, .block = 0 }, io, null, .{ .ctx = null, .run = &P.hook }, false, .none);
    try testing.expectEqual(@as(u64, 0), m.idx);
    try testing.expectEqual(@as(u64, 1), open.usedBlocks()); // the retry landed on the fresh pick
    try testing.expectEqual(@as(u32, 2), P.picks);
    try testing.expectEqual(@as(u32, 1), P.hook_runs);
    P.picks = 0;
    _ = try allocatePlaced(.{ .ctx = null, .pick = &P.pick }, .{ .ino = 1, .block = 0 }, io, null, .{ .ctx = null, .run = &P.hook }, true, .none);
    try testing.expectEqual(@as(u32, 1), P.hook_runs); // under a guard the hook never runs
    P.first = null;
    P.picks = 0;
    try testing.expectError(error.NoHealthyBackends, allocatePlaced(.{ .ctx = null, .pick = &P.pick }, .{ .ino = 1, .block = 0 }, io, null, null, false, .none));
}

test "stats JSON" {
    var buf: [512]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    try testing.expect(std.mem.startsWith(u8, w.buffered(), "{\"write_fresh_block_enospc_refusals\":"));
}
