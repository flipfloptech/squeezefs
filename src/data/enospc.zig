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
//! flight for 30 min, `.stats` in D-state). Who parks is the caller's
//! class (`SupplyWait`): the RAM-freeing flush parks while the allocator's
//! supply is VISIBLE (a grant window being refilled, a trim window, a
//! pending drain, the grace ring — the edges all ring `supply_edge`); the
//! fresh overlay mint never does (it declines to accumulation and yields
//! scarce supply to the flushes). With no reader plane the base bound is
//! 0 and the wall exactly 1 s (rung 9 F17).

const std = @import("std");
const testkit = @import("../core/testkit.zig");
const Io = std.Io;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const log = @import("../core/log.zig");
const derive = @import("../core/derive.zig");
const jc = @import("../meta/journal_core.zig");
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
    /// A write refused `ENOSPC` because the store's free bytes no longer
    /// cover the write cache's resident footprint plus this write's growth
    /// of it (`storeCoversRing`): the cache's claim on the store.
    pub var write_cache_claim_enospc_refusals: counters.ShardedCounter = .{};
    /// Full-store episodes at the promotion primitive (`FullStoreEpisode`).
    pub var full_store_episodes: counters.ShardedCounter = .{};
    pub var alloc_pressure_parks: counters.ShardedCounter = .{};
    pub var alloc_pressure_park_wall_refusals: counters.ShardedCounter = .{};
    pub var alloc_valve_passes: counters.ShardedCounter = .{};
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"write_fresh_block_enospc_refusals\":{d},\"write_cache_claim_enospc_refusals\":{d},\"full_store_episodes\":{d},\"alloc_pressure_parks\":{d},\"alloc_pressure_park_wall_refusals\":{d},\"alloc_valve_passes\":{d}}}", .{ stats.write_fresh_block_enospc_refusals.load(), stats.write_cache_claim_enospc_refusals.load(), stats.full_store_episodes.load(), stats.alloc_pressure_parks.load(), stats.alloc_pressure_park_wall_refusals.load(), stats.alloc_valve_passes.load() });
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

/// Some candidate's mint met `StorageFull` at or after `since_ns`: the
/// store ran out inside that span (the parked-buffer gate's wall tells
/// supply RETURNED to a full store from a plane refusing its flushes with
/// supply in sight).
pub fn spaceRefusedSince(candidates: []const *BlockAllocator, since_ns: u64) bool {
    for (candidates) |a| if (a.storageFullSince(since_ns)) return true;
    return false;
}

pub const WriteAdmission = enum {
    admit,
    /// `ENOSPC` before any byte is recorded; acked custody is unaffected.
    refuse_enospc,
};

/// THE WRITE CACHE'S CLAIM ON THE STORE (charter §6.2's terminal law, the
/// sync point's half): every resident of the ring is custody the next
/// promotion lands on a fresh block, so the ring's footprint is a claim on
/// the set's free bytes ahead of every later write. A write that would
/// GROW the footprint by `extra` B is admitted only while
/// `free ≥ ring + extra`; a write that creates fresh custody outside the
/// ring is admitted only while `free ≥ ring`. WHY: the ring admitted on its
/// own capacity alone — 25 % of RAM — and a filled store's small writes
/// were acked into it until the dismount pass FAILED them (generic/750:
/// 1,481 residents on a 16 GiB scratch; generic/475: a 7 GiB memfd), every
/// fsync of them answering EIO where the write should have answered
/// ENOSPC. The claim is bytes, not blocks: a packed promotion lands many
/// residents per block and an own-block one rounds up to a block, so the
/// footprint is the honest estimate between them; the sync point keeps the
/// terminal verdict for what the estimate admits.
pub fn storeCoversRing(free_bytes: u64, ring_bytes: u64, extra: u64) bool {
    return free_bytes >= ring_bytes +| extra;
}

/// THE FREE `df` SHOWS beside the write cache's claim: the store's free
/// bytes (whole blocks) less the claim rounded UP to the block — the room
/// whole-block mints may still take with the claim covered: for `k ≥ 1`,
/// `k × block ≤ freeBesideRing(free, ring, block)` ⟺
/// `storeCoversRing(free, ring, k × block)`, so a reservation of `df`'s
/// available fits beside the residents. WHY: shown in bytes, one 64 KiB
/// resident left `df` 4 MiB − 64 KiB above that room, and generic/103's
/// `falloc` of `df`'s available less 512 KiB needed the block the
/// resident's promotion lands on.
pub fn freeBesideRing(free_bytes: u64, ring_bytes: u64, block: u64) u64 {
    assert(block > 0);
    const ring_blocks = ring_bytes / block + @intFromBool(ring_bytes % block != 0);
    return free_bytes -| (ring_blocks *| block);
}

/// The write path's decision for the WHOLE write ahead of every block
/// future (FC:L20845-20881): fresh custody is refused while the set is
/// latched exhausted (`freshSupplyExhausted`) or while the write cache's
/// claim is uncovered (`storeCoversRing` false); custody the daemon already
/// holds keeps the ladder.
pub fn admitWrite(set_exhausted: bool, ring_uncovered: bool, write_would_create_fresh_custody: bool) WriteAdmission {
    if (!write_would_create_fresh_custody) return .admit;
    if (set_exhausted) {
        stats.write_fresh_block_enospc_refusals.inc();
        return .refuse_enospc;
    }
    if (ring_uncovered) {
        stats.write_cache_claim_enospc_refusals.inc();
        return .refuse_enospc;
    }
    return .admit;
}

/// The FULL-STORE EPISODE at the promotion primitive (the ring-full
/// latch's shape, `staging.Pool.RingFull`, 21a2308): ONE line when it
/// opens, one with the tally when it closes — never one per sync point or
/// per leg (generic/750: 235 `syncfs: a leg failed` and 79 `fsync …` error
/// lines in 19 s; generic/475: 1,085 and 3,176). Opened by the first
/// promotion the store refused for space (`refused`); a refusal never
/// closes it (a quiet application's hourly fsync is the same episode); a
/// promotion that LANDS closes it once the last refusal is a full park
/// wall old (`QUIET_NS`): a store under rewrite lands and refuses within
/// one wall — a free lands, the next mint takes it, the next fails — and an
/// episode closed by any landing logged one line per flap. The landing
/// side pays one relaxed load while no episode is open.
pub const FullStoreEpisode = struct {
    open: std.atomic.Value(bool) = .init(false),
    lock: jc.SpinLock = .{},
    since_ns: u64 = 0,
    last_refusal_ns: u64 = 0,
    refusals: u64 = 0,

    /// The park wall's floor: past it the store's own verdict stands.
    pub const QUIET_NS: u64 = PRESSURE_PARK_WALL_FLOOR_MS * std.time.ns_per_ms;

    pub const Closed = struct { since_ns: u64, refusals: u64 };

    /// A promotion the store refused for space: `true` iff it opened the
    /// episode (the caller's one line).
    pub fn refused(self: *FullStoreEpisode, now_ns: u64) bool {
        self.lock.lock();
        defer self.lock.unlock();
        self.last_refusal_ns = now_ns;
        if (self.open.load(.monotonic)) {
            self.refusals += 1;
            return false;
        }
        self.since_ns = now_ns;
        self.refusals = 1;
        self.open.store(true, .monotonic);
        stats.full_store_episodes.inc();
        return true;
    }

    /// A promotion landed: the episode's tally iff it closed (the last
    /// refusal older than `QUIET_NS`).
    pub fn landed(self: *FullStoreEpisode, now_ns: u64) ?Closed {
        if (!self.open.load(.monotonic)) return null;
        self.lock.lock();
        defer self.lock.unlock();
        if (!self.open.load(.monotonic) or now_ns -| self.last_refusal_ns < QUIET_NS) return null;
        self.open.store(false, .monotonic);
        return .{ .since_ns = self.since_ns, .refusals = self.refusals };
    }

    pub fn isOpen(self: *const FullStoreEpisode) bool {
        return self.open.load(.monotonic);
    }
};

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
                // A delete's corpses gathered upstream: their destroy starts
                // now, their frees join the hold this drain forces (a later
                // pass, the park's next slice, sees them).
                rr.kickUpstream();
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

/// Park on the allocator's supply edge until the open trim claim window
/// returns its blocks: a window is pending supply, never a deficit — a
/// full store under rewrite can hold its ENTIRE free list inside the
/// windows its trim passes claim, and refusing then was a spurious
/// TERMINAL ENOSPC (R7). THE CLASS: that regime's ordinary outcome, counted
/// `alloc_trim_window_parks` (≈ 0 off it; `alloc_trim_window_parked` while
/// parked), never a tripwire — the must-stay-0 it was fired under
/// generic/750 and generic/475 on a filled scratch, the fence-class ruling
/// of 2026-10-19 again. The window closes by the pass that opened it within
/// about one manners tick (`reclaim`'s pacing), so `ceiling_ns` passing
/// with it still open is the device stalled on its discards, not the
/// design: counted `alloc_trim_window_park_walls`, ONE warning per stall
/// episode on the volume (`trim_wall_warned`, cleared at the close), and
/// the park goes on — the window's blocks are supply in sight. Nothing
/// here is a "cannot happen": the caller's predicate is the only gate
/// (asserted), and a lost wake is the Notify's contract, not this site's.
fn awaitTrimReturn(alloc: *BlockAllocator, io: Io, ceiling_ns: u64) void {
    assert(alloc.trimClaimed() > 0);
    block_alloc.stats.alloc_trim_window_parks.inc();
    block_alloc.stats.alloc_trim_window_parked.add(1);
    defer block_alloc.stats.alloc_trim_window_parked.sub(1);
    const started = clock.monotonicNs();
    var deadline = started + ceiling_ns;
    var walled = false;
    while (alloc.trimClaimed() > 0) {
        var n = alloc.supply_edge.notified();
        _ = n.enable(io);
        if (alloc.trimClaimed() == 0) {
            n.cancel(io);
            break;
        }
        if (n.waitUntilUncancelable(io, deadline) == .deadline and alloc.trimClaimed() > 0) {
            block_alloc.stats.alloc_trim_window_park_walls.inc();
            walled = true;
            if (!alloc.trim_wall_warned.swap(true, .acq_rel)) log.warn("data volume {d}: an allocation has parked {d} ms on a trim claim window still open ({d} block(s) inside it, {d} parked) — a trim pass holds its window for about one manners tick, so the device is stalled on its discards; the store is not full, the park continues", .{ alloc.cfg.vol, (clock.monotonicNs() - started) / std.time.ns_per_ms, alloc.trimWindowedBlocks(), block_alloc.stats.alloc_trim_window_parked.load() });
            deadline = clock.monotonicNs() + ceiling_ns;
        }
    }
    if (walled) alloc.trim_wall_warned.store(false, .release);
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
            awaitTrimReturn(alloc, io, reclaim.PARK_BOUND_CEILING_MS * std.time.ns_per_ms);
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
    /// Supply gone invisible is the terminal verdict at once. Visible FOR
    /// the minting file (`freshSupplyVisibleFor`): a deleted file's own
    /// gathered corpse is no supply for its own bytes (decision 0003 §CW).
    visible_supply,
};

/// `allocate_block_grace_bounded`: the WRITE PATH's form. `.none` is
/// byte-identical to `allocateWithValve` (one branch). `.visible_supply`:
/// while the allocator's supply is visible to a mint for `for_ino` (the
/// file the block is for; null names none), park `slice` at a time on the
/// supply edge (the top-up, the free, the trim return and the drain ring
/// it) up to the WALL, then refuse loud; invisible ⇒ terminal at once.
pub fn allocateBlockGraceBounded(alloc: *BlockAllocator, io: Io, valve: ?Valve, wait: SupplyWait, fence_bound: ?FenceBoundSource, for_ino: ?u64) AllocError!Minted {
    const started = clock.monotonicNs();
    while (true) {
        const err = if (allocateWithValve(alloc, io, valve)) |m| return m else |e| e;
        if (err != error.StorageFull or wait == .none or !alloc.freshSupplyVisibleFor(for_ino)) return err;
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
    const err = if (allocateBlockGraceBounded(first, io, valve, wait, null, place.ino)) |m| return .{ .alloc = first, .minted = m } else |e| e;
    if (err != error.StorageFull) return err;
    if (!under_guard) {
        if (hook) |h| _ = h.run(h.ctx);
    }
    const second = picker.pick(picker.ctx, place) orelse return error.NoHealthyBackends;
    return .{ .alloc = second, .minted = try allocateBlockGraceBounded(second, io, valve, wait, null, place.ino) };
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
    const claims = stats.write_cache_claim_enospc_refusals.load();
    try testing.expectEqual(WriteAdmission.refuse_enospc, admitWrite(true, false, true));
    try testing.expectEqual(WriteAdmission.admit, admitWrite(true, false, false)); // a mapped block's rewrite lands in place
    try testing.expectEqual(WriteAdmission.admit, admitWrite(false, false, true));
    try testing.expectEqual(refusals + 1, stats.write_fresh_block_enospc_refusals.load());
    // The write cache's claim: uncovered ⇒ fresh custody refused, counted apart; held custody admitted.
    try testing.expectEqual(WriteAdmission.refuse_enospc, admitWrite(false, true, true));
    try testing.expectEqual(WriteAdmission.admit, admitWrite(false, true, false));
    try testing.expectEqual(claims + 1, stats.write_cache_claim_enospc_refusals.load());
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

/// A full store under rewrite: every block allocated, the one freed block
/// claimed by a trim pass — the free list is entirely inside the window.
fn fullStoreInsideTrimWindow() !struct { alloc: *BlockAllocator, guard: block_alloc.InflightAllocGuard } {
    const a = try mkAlloc(2, 0);
    errdefer a.deinit();
    _ = try a.allocateBlock();
    _ = try a.allocateBlock();
    _ = try a.freeBlock(1);
    const g = a.claimFreeForTrim(1).?;
    try testing.expectEqual(@as(u64, 0), a.freeSupplyBlocks());
    try testing.expect(a.freshSupplyVisible()); // the window IS supply
    return .{ .alloc = a, .guard = g };
}

test "THE CLASS OF A TRIM-WINDOW PARK (the fence-class ruling applied): a full store under rewrite with its free list inside a trim claim window parks the allocation and lands it on the returned block — counted alloc_trim_window_parks, the gauge 1 while parked and 0 after, NO tripwire, NO error line (red: the must-stay-0 fired and logged an INVARIANT TRIPWIRE line — generic/750 ×3, generic/475 ×7)" {
    const io = testing.io;
    var f = try fullStoreInsideTrimWindow();
    const a = f.alloc;
    defer a.deinit();
    const Returner = struct {
        fn run(io_: Io, alloc: *BlockAllocator, guard: *block_alloc.InflightAllocGuard, parked: *u64) void {
            while (alloc.supply_edge.waiters(io_) == 0) std.Thread.yield() catch {};
            parked.* = block_alloc.stats.alloc_trim_window_parked.load();
            alloc.returnFromTrim(1, guard);
            alloc.supply_edge.notifyWaiters(io_);
        }
    };
    var parked_during: u64 = 0;
    var fut = io.concurrent(Returner.run, .{ io, a, &f.guard, &parked_during }) catch |err| switch (err) {
        error.ConcurrencyUnavailable => return testkit.skip(@src(), .no_concurrency, "the executor refused a unit of concurrency"),
    };
    const parks0 = block_alloc.stats.alloc_trim_window_parks.load();
    const walls0 = block_alloc.stats.alloc_trim_window_park_walls.load();
    const trips0 = counters.invariantTripwires();
    const m = try allocateWithValve(a, io, null);
    fut.await(io);
    try testing.expectEqual(@as(u64, 1), m.idx);
    try testing.expectEqual(parks0 + 1, block_alloc.stats.alloc_trim_window_parks.load());
    try testing.expectEqual(walls0, block_alloc.stats.alloc_trim_window_park_walls.load());
    try testing.expectEqual(@as(u64, 1), parked_during);
    try testing.expectEqual(@as(u64, 0), block_alloc.stats.alloc_trim_window_parked.load());
    try testing.expectEqual(trips0, counters.invariantTripwires());
    try testing.expectEqual(@as(u64, 0), counters.tripwireCount("alloc_trim_window_parks"));
}

test "a trim-window park past its ceiling with the window STILL open is the device stalled, not the store full: counted alloc_trim_window_park_walls (one per ceiling), ONE warning per episode on the volume, the park goes on and lands on the returned block; the latch clears at the close" {
    const io = testing.io;
    var f = try fullStoreInsideTrimWindow();
    const a = f.alloc;
    defer a.deinit();
    const ceiling_ns: u64 = 20 * std.time.ns_per_ms;
    const parks0 = block_alloc.stats.alloc_trim_window_parks.load();
    const walls0 = block_alloc.stats.alloc_trim_window_park_walls.load();
    const trips0 = counters.invariantTripwires();
    const Returner = struct {
        fn run(io_: Io, alloc: *BlockAllocator, guard: *block_alloc.InflightAllocGuard, walls_before: u64) void {
            // The window outlives three ceilings before the pass returns it.
            while (block_alloc.stats.alloc_trim_window_park_walls.load() < walls_before + 3) std.Thread.yield() catch {};
            while (alloc.supply_edge.waiters(io_) == 0) std.Thread.yield() catch {};
            alloc.returnFromTrim(1, guard);
            alloc.supply_edge.notifyWaiters(io_);
        }
    };
    var fut = io.concurrent(Returner.run, .{ io, a, &f.guard, walls0 }) catch |err| switch (err) {
        error.ConcurrencyUnavailable => return testkit.skip(@src(), .no_concurrency, "the executor refused a unit of concurrency"),
    };
    const t0 = clock.monotonicNs();
    awaitTrimReturn(a, io, ceiling_ns);
    fut.await(io);
    try testing.expect(clock.monotonicNs() - t0 >= 3 * ceiling_ns);
    try testing.expectEqual(parks0 + 1, block_alloc.stats.alloc_trim_window_parks.load()); // one park spanning the walls
    try testing.expect(block_alloc.stats.alloc_trim_window_park_walls.load() >= walls0 + 3);
    try testing.expectEqual(@as(u64, 0), a.trimClaimed());
    try testing.expect(!a.trim_wall_warned.load(.acquire));
    try testing.expectEqual(trips0, counters.invariantTripwires());
    const m = try a.allocateBlock();
    try testing.expectEqual(@as(u64, 1), m.idx);
}

test "the write path's grace-bounded mint: `.none` ⇒ byte-identical to the valve; `.visible_supply` on an exhausted set ⇒ terminal at once; with supply in sight ⇒ parks slices up to the wall then refuses loud; a free during the park lands the mint" {
    const io = testing.io;
    const a = try mkAlloc(1, 0);
    defer a.deinit();
    _ = try a.allocateBlock();
    const parks0 = stats.alloc_pressure_parks.load();
    const wall0 = stats.alloc_pressure_park_wall_refusals.load();
    try testing.expectError(error.StorageFull, allocateBlockGraceBounded(a, io, null, .none, null, null));
    try testing.expectEqual(parks0, stats.alloc_pressure_parks.load()); // `.none` never parks
    // Nothing in sight: the terminal verdict at once, no park.
    try testing.expectError(error.StorageFull, allocateBlockGraceBounded(a, io, null, .visible_supply, null, null));
    try testing.expectEqual(parks0, stats.alloc_pressure_parks.load());
    // Supply in sight that never arrives (a reclaim drain "pending" forever) with the floor wall: slices then the wall refusal.
    const Pending = struct {
        fn pending(_: ?*anyopaque, _: ?u64) bool {
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
    try testing.expectError(error.StorageFull, allocateBlockGraceBounded(a, io, null, .visible_supply, .{ .ctx = null, .baseMs = &Src.base }, null));
    try log.testing.expectAllFired();
    const waited = clock.monotonicNs() - t0;
    try testing.expect(waited >= PRESSURE_PARK_WALL_FLOOR_MS * std.time.ns_per_ms);
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
    const m = try allocateBlockGraceBounded(a, io, null, .visible_supply, null, null);
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

test "THE WRITE CACHE'S CLAIM ON THE STORE: the ring's footprint plus the write's growth of it must fit the set's free bytes — a new resident on a store with exactly its bytes free is admitted, one byte more is refused; a resident's same-size re-stage grows nothing; an uncovered ring refuses every fresh custody; the arithmetic saturates" {
    try testing.expect(storeCoversRing(1 << 20, 0, 1 << 20));
    try testing.expect(!storeCoversRing(1 << 20, 0, (1 << 20) + 1));
    try testing.expect(storeCoversRing(1 << 20, 1 << 20, 0)); // the re-stage at the same cost
    try testing.expect(!storeCoversRing(1 << 20, 1 << 20, 1)); // one byte of growth
    try testing.expect(!storeCoversRing(10, 11, 0)); // the store shrank under the ring: no fresh custody
    try testing.expect(storeCoversRing(0, 0, 0)); // an empty ring claims nothing
    try testing.expect(!storeCoversRing(std.math.maxInt(u64) - 1, std.math.maxInt(u64), 1));
}

test "THE FREE `df` SHOWS beside the ring (decision 0003 §CZ): the free blocks less the claim rounded up to the block — an empty ring shows every block, a 64 KiB resident one block fewer, a claim one byte past a block two; it saturates; and it is exactly what whole-block mints are admitted against: `k × block ≤ shown` ⟺ `storeCoversRing(free, ring, k × block)` for every `k ≥ 1`" {
    const b: u64 = 4 << 20;
    try testing.expectEqual(10 * b, freeBesideRing(10 * b, 0, b));
    try testing.expectEqual(9 * b, freeBesideRing(10 * b, 64 << 10, b));
    try testing.expectEqual(9 * b, freeBesideRing(10 * b, b, b));
    try testing.expectEqual(8 * b, freeBesideRing(10 * b, b + 1, b));
    try testing.expectEqual(@as(u64, 0), freeBesideRing(b, 64 << 10, b)); // generic/103's last block is the resident's
    try testing.expectEqual(@as(u64, 0), freeBesideRing(2 * b, 3 * b, b)); // an over-committed ring shows nothing
    try testing.expectEqual(@as(u64, 0), freeBesideRing(b, std.math.maxInt(u64), b));
    const rings = [_]u64{ 0, 1, 4095, 4096, 64 << 10, b - 1, b, b + 1, 2 * b, 3 * b - 7 };
    var free_blocks: u64 = 0;
    while (free_blocks <= 6) : (free_blocks += 1) {
        for (rings) |ring| {
            var k: u64 = 1;
            while (k <= 7) : (k += 1) {
                const free = free_blocks * b;
                try testing.expectEqual(storeCoversRing(free, ring, k * b), k * b <= freeBesideRing(free, ring, b));
            }
        }
    }
}

test "THE FULL-STORE EPISODE is one open and one close: the first refusal opens it (counted full_store_episodes), later refusals and interleaved landings within the quiet wall keep it open, a landing a full wall after the last refusal closes it with the tally; a refusal after the close opens the next" {
    var e: FullStoreEpisode = .{};
    const episodes0 = stats.full_store_episodes.load();
    const t0: u64 = 10 * std.time.ns_per_s;
    try testing.expect(!e.isOpen());
    try testing.expectEqual(@as(?FullStoreEpisode.Closed, null), e.landed(t0)); // nothing open: the landing side's one load
    try testing.expect(e.refused(t0));
    try testing.expect(e.isOpen());
    try testing.expectEqual(episodes0 + 1, stats.full_store_episodes.load());
    // The store under rewrite: a free lands, the next mint takes it, the next fails — the episode, not its end.
    try testing.expectEqual(@as(?FullStoreEpisode.Closed, null), e.landed(t0 + 1));
    try testing.expect(!e.refused(t0 + 2));
    try testing.expectEqual(@as(?FullStoreEpisode.Closed, null), e.landed(t0 + 2 + FullStoreEpisode.QUIET_NS - 1));
    // A quiet application: its refusals an hour apart are the same episode.
    try testing.expect(!e.refused(t0 + 3600 * std.time.ns_per_s));
    try testing.expectEqual(episodes0 + 1, stats.full_store_episodes.load());
    // The close: a landing a full wall after the last refusal.
    const closed = e.landed(t0 + 3600 * std.time.ns_per_s + FullStoreEpisode.QUIET_NS).?;
    try testing.expectEqual(t0, closed.since_ns);
    try testing.expectEqual(@as(u64, 3), closed.refusals);
    try testing.expect(!e.isOpen());
    // The next refusal is the next episode.
    try testing.expect(e.refused(t0 + 7200 * std.time.ns_per_s));
    try testing.expectEqual(episodes0 + 2, stats.full_store_episodes.load());
}

test "stats JSON" {
    var buf: [512]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    try testing.expect(std.mem.startsWith(u8, w.buffered(), "{\"write_fresh_block_enospc_refusals\":"));
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"write_cache_claim_enospc_refusals\":") != null);
}
