//! The free-grace ring (charter §5.5 "the free-grace ring"; `docs/porting/
//! sym_data_planes.md` §4.2; 0002 row 242). PORTABLE.
//!
//! WHY: the read path's serve proof — "bytes for key K serve for block b
//! iff the fetch was incarnation-valid and the current map still binds b →
//! K" — is process-local; a reader whose cached map still binds `b → K`
//! serves another file's bytes after the writer freed and REISSUED the
//! offset, silently on a passthrough volume. The ring refuses to reallocate
//! an offset until every registered reader has acknowledged passing that
//! epoch, riding the membership beat. A reader that fails to acknowledge
//! is FENCED, not waited on. It is NOT the S7 quarantine: admission is
//! every terminal free (vs a death event), release a monotone bound (vs a
//! drain proof), FIFO (vs a cohort set); they compose in a fixed order —
//! custody proof first, reader coherence second.
//!
//! The LABEL is the owner's monotonic ms — an owner-minted CAUSAL label a
//! reader only ever echoes (never a wall clock the reader reads itself);
//! the published BOUND is the minimum acknowledged label over live members
//! (`maxInt` = nobody can hold a stale binding). Under GPFS-strict tokens
//! the ring is the TIMEOUT path alone: the freeing publish RECALLED every
//! reader's token before it committed, so the recall gate answers `gated`
//! and the offset publishes directly; a LIVE member's recall unacked past
//! the bound opens a `T_owner` window during which frees ride the ring.
//! The reader-ack ladder's ten A/B levers are not carried (0002 row 242):
//! only the ring, the recall gate and the laggard fence ship.

const std = @import("std");
const Atomic = std.atomic.Value;
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const counters = @import("../../core/counters.zig");
const knobs = @import("../../core/knobs.zig");
const derive = @import("../../core/derive.zig");
const log = @import("../../core/log.zig");
const jc = @import("../../meta/journal_core.zig");
const block_alloc = @import("../block_alloc.zig");
const enospc = @import("../enospc.zig");

// ---------------------------------------------------------------------------
// Constants and derivations (every one with its WHY).
// ---------------------------------------------------------------------------

/// `(label, offset, size)`: three u64 words per entry.
pub const GRACE_ENTRY_BYTES: u64 = 24;
/// The measured saturated ingest of 12.7 GB/s over one default
/// acknowledgement cycle (≈ 38 s) displaces ≈ 120 k blocks: a ring smaller
/// than that would force readers under a healthy stream.
pub const RING_CAP_FLOOR: u64 = 131_072;
/// The harvest quantum = the block-reclaim drain's batch
/// (`SQUEEZEFS_RECLAIM_BATCH_BLOCKS`).
pub const HARVEST_BATCH: usize = knobs.intDefault("SQUEEZEFS_RECLAIM_BATCH_BLOCKS", usize);
/// The owner TTL (0002 row 188).
pub const T_OWNER_MS: u64 = 45_000;

/// `max(budget / 1024 / 24, floor)`: < 0.1 % of the budget, NOT an R5
/// component (it cannot shed).
pub fn ringCapDerived(budget_bytes: u64) u64 {
    return @max(budget_bytes / 1024 / GRACE_ENTRY_BYTES, RING_CAP_FLOOR);
}

pub fn resolveRingCap(explicit: ?u64, derived: u64) u64 {
    return explicit orelse derived;
}

/// `3·beat + 3·staleness + skew + D_purge`: learn a label, carry the ack
/// home, the owner's publish cadence; qualify, wait for that pass, expire
/// the daemon caches.
pub fn ackCycleMs(renew_interval_ms: u64, staleness_bound_ms: u64, skew_max_ms: u64, d_purge_ms: u64) u64 {
    return 3 * renew_interval_ms + 3 * staleness_bound_ms + skew_max_ms + d_purge_ms;
}

/// The routine fence bound = 2 × cycle: one missed cycle tolerated,
/// mirroring S6's three attempts.
pub fn routineFenceBoundMs(ack_cycle_ms: u64) u64 {
    return 2 * ack_cycle_ms;
}

/// The pressure bound = one cycle.
pub fn pressureBoundMs(ack_cycle_ms: u64) u64 {
    return ack_cycle_ms;
}

pub const BoundError = error{BelowOneCycle};

/// An explicit `SQUEEZEFS_FREE_GRACE_MAX_MS` below ONE cycle REFUSES
/// ("never clamped"); absent ⇒ the routine bound.
pub fn resolveMaxMs(explicit: ?u64, ack_cycle_ms: u64) BoundError!u64 {
    const e = explicit orelse return routineFenceBoundMs(ack_cycle_ms);
    if (e < ack_cycle_ms) return error.BelowOneCycle;
    return e;
}

/// The valve's runway: `min(cap − held, free supply) × span / held` ms —
/// how long the ring's holdings leave the allocator before it must force.
pub fn runwayMs(cap: u64, held: u64, free_supply: u64, span_ms: u64) u64 {
    if (held == 0) return std.math.maxInt(u64);
    const room = @min(cap -| held, free_supply);
    return @intCast((@as(u128, room) * span_ms) / held);
}

/// Rung (b): the deadline slides linearly from the routine bound to the
/// pressure bound as the runway shrinks below the routine bound.
pub fn effectiveBoundMs(routine_ms: u64, pressure_ms: u64, runway_ms: u64) u64 {
    if (runway_ms >= routine_ms) return routine_ms;
    return @max(pressure_ms, runway_ms);
}

// ---------------------------------------------------------------------------
// Stats (§7.2).
// ---------------------------------------------------------------------------

pub const stats = struct {
    pub var free_grace_deferrals: counters.ShardedCounter = .{};
    pub var free_grace_releases: counters.ShardedCounter = .{};
    pub var free_grace_forced_releases: counters.ShardedCounter = .{};
    pub var free_grace_laggard_fences: counters.ShardedCounter = .{};
    pub var free_grace_timeout_deferrals: counters.ShardedCounter = .{};
    pub var free_grace_reader_acks: counters.ShardedCounter = .{};
    pub var free_grace_recall_gated_frees: counters.ShardedCounter = .{};
    pub var free_grace_recall_timeout_deferrals: counters.ShardedCounter = .{};
    pub var free_grace_over_cap_refusals: counters.ShardedCounter = .{};
    pub var free_grace_residence_ms: counters.ShardedLatencyHistogram = .{};
    /// Levels.
    pub var free_grace_offsets: counters.Gauge = .{};
    pub var free_grace_bytes: counters.Gauge = .{};
    pub var free_grace_ring_cap: counters.Gauge = .{};
    pub var free_grace_effective_bound_ms: counters.Gauge = .{};
};

// ---------------------------------------------------------------------------
// The seams.
// ---------------------------------------------------------------------------

/// The owner's monotonic clock in ms (the port's S6 owner clock).
pub const LabelClock = struct {
    ctx: ?*anyopaque,
    nowMs: *const fn (?*anyopaque) u64,
};

/// Evict the laggard through S6 (the reader that never acknowledged).
pub const LaggardFence = struct {
    ctx: ?*anyopaque,
    fence: *const fn (?*anyopaque, member: u32) void,
};

pub const RecallVerdict = enum {
    /// Every reader's token was recalled before the freeing publish
    /// committed: publish directly.
    gated,
    /// A live member's recall is unacked past the bound: the ring is the
    /// timeout path.
    open,
};

/// The token plane's gate (the port's token plane binds it).
pub const RecallGate = struct {
    ctx: ?*anyopaque,
    verdict: *const fn (?*anyopaque) RecallVerdict,
};

/// The most members one ring tracks acks for: the appender-directory
/// ceiling a single volume's reader population can reach in rung 6.
pub const MEMBERS_MAX: usize = 256;

pub const GraceEntry = struct { label: u64, idx: u64, bytes: u64 };

pub const Config = struct {
    cap: u64,
    routine_bound_ms: u64,
    pressure_bound_ms: u64,
    clock: LabelClock,
    fence: ?LaggardFence = null,
    recall_gate: ?RecallGate = null,
    /// `SQUEEZEFS_FREE_GRACE_VALVE`: rungs (a) + (b) — the sliding
    /// deadline; off = the routine bound alone.
    valve: bool = true,
};

pub const GraceRing = struct {
    gpa: Allocator,
    cfg: Config,
    lock: jc.SpinLock = .{},
    entries: []GraceEntry,
    head: usize = 0,
    len_locked: usize = 0,
    /// Published lock-free.
    len: Atomic(u64) = .init(0),
    bytes: Atomic(u64) = .init(0),
    armed: Atomic(bool) = .init(false),
    /// The minimum acknowledged label over live members; `maxInt` = nobody.
    bound: Atomic(u64) = .init(std.math.maxInt(u64)),
    /// Per-member last acknowledged label (0 = never acked / not live).
    acks: [MEMBERS_MAX]u64 = @splat(0),
    live: [MEMBERS_MAX]bool = @splat(false),
    /// The recall-timeout window end (0 = closed).
    recall_window_until_ms: Atomic(u64) = .init(0),
    // Per-ring closure: `deferrals ≡ releases + len`.
    deferrals: Atomic(u64) = .init(0),
    releases: Atomic(u64) = .init(0),
    timeout_deferrals: Atomic(u64) = .init(0),

    pub fn init(gpa: Allocator, cfg: Config) Allocator.Error!*GraceRing {
        assert(cfg.cap > 0);
        const self = try gpa.create(GraceRing);
        errdefer gpa.destroy(self);
        const entries = try gpa.alloc(GraceEntry, @intCast(cfg.cap));
        self.* = .{ .gpa = gpa, .cfg = cfg, .entries = entries };
        stats.free_grace_ring_cap.set(cfg.cap);
        return self;
    }

    pub fn deinit(self: *GraceRing) void {
        self.gpa.free(self.entries);
        self.gpa.destroy(self);
    }

    /// Arm when the first foreign reader registers; disarmed, `defer_` is
    /// one relaxed load and false.
    pub fn arm(self: *GraceRing) void {
        self.armed.store(true, .release);
    }

    pub fn disarm(self: *GraceRing) void {
        self.armed.store(false, .release);
    }

    fn nowMs(self: *const GraceRing) u64 {
        return self.cfg.clock.nowMs(self.cfg.clock.ctx);
    }

    // ---- members and the bound ----

    pub fn memberJoin(self: *GraceRing, member: u32) void {
        self.lock.lock();
        defer self.lock.unlock();
        self.live[member] = true;
        self.acks[member] = 0;
        self.recomputeBoundLocked();
    }

    pub fn memberLeave(self: *GraceRing, member: u32) void {
        self.lock.lock();
        defer self.lock.unlock();
        self.live[member] = false;
        self.recomputeBoundLocked();
    }

    /// A reader echoes the label it was handed (causal, never temporal).
    pub fn noteAck(self: *GraceRing, member: u32, label: u64) void {
        self.lock.lock();
        defer self.lock.unlock();
        if (!self.live[member]) return;
        self.acks[member] = @max(self.acks[member], label);
        stats.free_grace_reader_acks.inc();
        self.recomputeBoundLocked();
    }

    fn recomputeBoundLocked(self: *GraceRing) void {
        var b: u64 = std.math.maxInt(u64);
        for (self.live, 0..) |l, i| if (l) {
            b = @min(b, self.acks[i]);
        };
        self.bound.store(b, .release);
    }

    pub fn publishedBound(self: *const GraceRing) u64 {
        return self.bound.load(.acquire);
    }

    // ---- records ----

    /// Unarmed ⇒ false (the caller publishes as always); recall-gated ⇒
    /// false (the token recall was the qualification); armed ⇒ push
    /// `{label, idx, bytes}`; a full ring refuses the deferral (the caller
    /// publishes — the valve's rung (c) should have forced long before).
    pub fn defer_(self: *GraceRing, idx: u64, bytes: u64) bool {
        if (!self.armed.load(.monotonic)) return false;
        if (self.cfg.recall_gate) |g| if (g.verdict(g.ctx) == .gated and self.recall_window_until_ms.load(.monotonic) <= self.nowMs()) {
            stats.free_grace_recall_gated_frees.inc();
            return false;
        };
        const label = self.nowMs();
        self.lock.lock();
        defer self.lock.unlock();
        if (self.len_locked == self.entries.len) {
            stats.free_grace_over_cap_refusals.inc();
            return false;
        }
        self.entries[(self.head + self.len_locked) % self.entries.len] = .{ .label = label, .idx = idx, .bytes = bytes };
        self.len_locked += 1;
        self.len.store(self.len_locked, .release);
        _ = self.bytes.fetchAdd(bytes, .monotonic);
        _ = self.deferrals.fetchAdd(1, .monotonic);
        stats.free_grace_deferrals.inc();
        stats.free_grace_offsets.set(self.len_locked);
        stats.free_grace_bytes.set(self.bytes.load(.monotonic));
        if (self.recall_window_until_ms.load(.monotonic) > label) {
            _ = self.timeout_deferrals.fetchAdd(1, .monotonic);
            stats.free_grace_recall_timeout_deferrals.inc();
        }
        return true;
    }

    /// A LIVE member's recall unacked past the bound opens a `T_owner`
    /// window on the owner clock during which frees ride the ring.
    pub fn noteRecallUnackedLive(self: *GraceRing) void {
        _ = self.recall_window_until_ms.fetchMax(self.nowMs() + T_OWNER_MS, .acq_rel);
    }

    /// Pop the front while `label ≤ bound`, up to `max`; if the oldest is
    /// above the bound and the ring is over cap or its deadline expired,
    /// `force_progress`: the laggard is fenced, the bound recomputed, the
    /// forced releases counted. The popped offsets are OWED a free-list
    /// publish by the allocator.
    pub fn harvest(self: *GraceRing, max: usize, out: []u64) usize {
        if (!self.armed.load(.monotonic)) return 0;
        const now = self.nowMs();
        self.lock.lock();
        defer self.lock.unlock();
        var n: usize = 0;
        while (n < max and n < out.len and self.len_locked > 0) {
            const e = self.entries[self.head];
            if (e.label > self.bound.load(.monotonic)) {
                const over_cap = self.len_locked >= self.entries.len;
                const deadline = self.deadlineLocked(e.label);
                if (!over_cap and now < deadline) break;
                self.forceProgressLocked(e.label, if (over_cap) "over cap" else "deadline");
                if (e.label > self.bound.load(.monotonic)) break;
            }
            out[n] = e.idx;
            n += 1;
            self.head = (self.head + 1) % self.entries.len;
            self.len_locked -= 1;
            _ = self.bytes.fetchSub(e.bytes, .monotonic);
            _ = self.releases.fetchAdd(1, .monotonic);
            stats.free_grace_releases.inc();
            stats.free_grace_residence_ms.record((now -| e.label) * std.time.ns_per_ms);
        }
        self.len.store(self.len_locked, .release);
        stats.free_grace_offsets.set(self.len_locked);
        stats.free_grace_bytes.set(self.bytes.load(.monotonic));
        return n;
    }

    /// The oldest entry's deadline: its label plus the effective bound
    /// (rung (b) slides it toward the pressure bound as the ring fills).
    fn deadlineLocked(self: *GraceRing, label: u64) u64 {
        var bound = self.cfg.routine_bound_ms;
        if (self.cfg.valve) {
            // The runway in ms: room × routine / held — a full ring has none.
            const held: u64 = self.len_locked;
            const runway = runwayMs(self.entries.len, held, self.entries.len -| held, self.cfg.routine_bound_ms);
            bound = effectiveBoundMs(self.cfg.routine_bound_ms, self.cfg.pressure_bound_ms, runway);
        }
        stats.free_grace_effective_bound_ms.set(bound);
        return label + bound;
    }

    /// Fence every live member whose ack is below `label` (the laggards),
    /// recompute the bound; the releases that follow are FORCED.
    fn forceProgressLocked(self: *GraceRing, label: u64, why: []const u8) void {
        var fenced: u32 = 0;
        for (self.live, 0..) |l, i| if (l and self.acks[i] < label) {
            self.live[i] = false;
            fenced += 1;
            stats.free_grace_laggard_fences.inc();
            if (self.cfg.fence) |f| f.fence(f.ctx, @intCast(i));
        };
        self.recomputeBoundLocked();
        _ = self.timeout_deferrals.fetchAdd(1, .monotonic);
        stats.free_grace_timeout_deferrals.inc();
        stats.free_grace_forced_releases.inc();
        log.warn("free-grace ring: forced progress ({s}) past label {d}: {d} laggard reader(s) fenced; the bound is now {d}", .{ why, label, fenced, self.bound.load(.monotonic) });
    }

    /// A bounded scan (diagnostics, the debt drainer's skip, fsck).
    pub fn holds(self: *GraceRing, idx: u64) bool {
        if (!self.armed.load(.monotonic)) return false;
        self.lock.lock();
        defer self.lock.unlock();
        var i: usize = 0;
        while (i < self.len_locked) : (i += 1) if (self.entries[(self.head + i) % self.entries.len].idx == idx) return true;
        return false;
    }

    pub fn length(self: *const GraceRing) u64 {
        return self.len.load(.acquire);
    }

    pub fn oldestLabel(self: *GraceRing) ?u64 {
        self.lock.lock();
        defer self.lock.unlock();
        if (self.len_locked == 0) return null;
        return self.entries[self.head].label;
    }

    /// `deferrals ≡ releases + len`.
    pub fn closureHolds(self: *const GraceRing) bool {
        return self.deferrals.load(.acquire) == self.releases.load(.acquire) + self.len.load(.acquire);
    }

    // ---- the allocator's `GraceRing` seam ----

    fn seamDefer(ctx: *anyopaque, idx: u64, bytes: u64) bool {
        const r: *GraceRing = @ptrCast(@alignCast(ctx));
        return r.defer_(idx, bytes);
    }
    fn seamHarvest(ctx: *anyopaque, max: usize, out: []u64) usize {
        const r: *GraceRing = @ptrCast(@alignCast(ctx));
        return r.harvest(max, out);
    }
    fn seamHolds(ctx: *anyopaque, idx: u64) bool {
        const r: *GraceRing = @ptrCast(@alignCast(ctx));
        return r.holds(idx);
    }
    fn seamLen(ctx: *anyopaque) u64 {
        const r: *GraceRing = @ptrCast(@alignCast(ctx));
        return r.length();
    }
    const seam_vtable: block_alloc.GraceRing.VTable = .{ .defer_ = &seamDefer, .harvest = &seamHarvest, .holds = &seamHolds, .len = &seamLen };

    pub fn allocatorSeam(self: *GraceRing) block_alloc.GraceRing {
        return .{ .ctx = self, .vtable = &seam_vtable };
    }

    // ---- the ENOSPC law's fence-bound source ----

    fn baseMs(ctx: ?*anyopaque) u64 {
        const r: *GraceRing = @ptrCast(@alignCast(ctx.?));
        return r.cfg.routine_bound_ms;
    }

    pub fn fenceBoundSource(self: *GraceRing) enospc.FenceBoundSource {
        return .{ .ctx = self, .baseMs = &baseMs };
    }
};

/// The knob faces, read once at the arm: `SQUEEZEFS_FREE_GRACE_MAX_MS`
/// (refuses below one cycle), `SQUEEZEFS_FREE_GRACE_MAX_OFFSETS`,
/// `SQUEEZEFS_FREE_GRACE_VALVE`.
pub const Knobs = struct {
    max_ms: ?u64,
    max_offsets: ?u64,
    valve: bool,

    pub fn fromEnv(env: *const std.process.Environ.Map) Knobs {
        return .{
            .max_ms = knobs.optIntKnob("SQUEEZEFS_FREE_GRACE_MAX_MS", u64, env),
            .max_offsets = knobs.optIntKnob("SQUEEZEFS_FREE_GRACE_MAX_OFFSETS", u64, env),
            .valve = knobs.boolKnob("SQUEEZEFS_FREE_GRACE_VALVE", env),
        };
    }

    /// The resolved config over the derived terms.
    pub fn resolve(self: Knobs, budget_bytes: u64, ack_cycle_ms: u64, clock: LabelClock) BoundError!Config {
        return .{
            .cap = resolveRingCap(self.max_offsets, ringCapDerived(budget_bytes)),
            .routine_bound_ms = try resolveMaxMs(self.max_ms, ack_cycle_ms),
            .pressure_bound_ms = pressureBoundMs(ack_cycle_ms),
            .clock = clock,
            .valve = self.valve,
        };
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

const FakeClock = struct {
    now: u64 = 1000,
    fn nowMs(ctx: ?*anyopaque) u64 {
        const c: *FakeClock = @ptrCast(@alignCast(ctx.?));
        return c.now;
    }
    fn clock(self: *FakeClock) LabelClock {
        return .{ .ctx = self, .nowMs = &nowMs };
    }
};

const FenceLog = struct {
    fenced: [8]u32 = @splat(0),
    n: usize = 0,
    fn fence(ctx: ?*anyopaque, member: u32) void {
        const f: *FenceLog = @ptrCast(@alignCast(ctx.?));
        f.fenced[f.n] = member;
        f.n += 1;
    }
};

test "derivations: the ring cap, the ack cycle, the two bounds, the explicit max below one cycle REFUSES, the runway and the sliding deadline" {
    try testing.expectEqual(RING_CAP_FLOOR, ringCapDerived(1 << 30)); // 1 GiB / 1024 / 24 = 43 k < floor
    try testing.expectEqual(@as(u64, (64 << 30) / 1024 / 24), ringCapDerived(64 << 30));
    try testing.expectEqual(@as(u64, 7), resolveRingCap(7, 1000));
    try testing.expectEqual(@as(u64, 38_000), ackCycleMs(10_000, 2_000, 1_000, 1_000)); // ≈ 38 s shipped
    try testing.expectEqual(@as(u64, 76_000), routineFenceBoundMs(38_000));
    try testing.expectEqual(@as(u64, 38_000), pressureBoundMs(38_000));
    try testing.expectEqual(@as(u64, 76_000), try resolveMaxMs(null, 38_000));
    try testing.expectEqual(@as(u64, 40_000), try resolveMaxMs(40_000, 38_000));
    try testing.expectError(error.BelowOneCycle, resolveMaxMs(37_999, 38_000));
    try testing.expectEqual(@as(u64, 500), runwayMs(1000, 500, 10_000, 500)); // room 500 × 500 / 500
    try testing.expectEqual(@as(u64, 100), runwayMs(1000, 500, 100, 500)); // the free supply caps the room
    try testing.expectEqual(std.math.maxInt(u64), runwayMs(1000, 0, 0, 500));
    try testing.expectEqual(@as(u64, 76_000), effectiveBoundMs(76_000, 38_000, 100_000));
    try testing.expectEqual(@as(u64, 50_000), effectiveBoundMs(76_000, 38_000, 50_000));
    try testing.expectEqual(@as(u64, 38_000), effectiveBoundMs(76_000, 38_000, 1_000));
    try testing.expectEqual(@as(usize, 64), HARVEST_BATCH);
    try testing.expectEqual(@as(u64, 24), GRACE_ENTRY_BYTES);
}

test "free_grace_closure_holds_per_ring_on_two_hand_made_rings: unarmed defers nothing; armed defers with the owner label; the harvest pops while label ≤ bound; deferrals ≡ releases + len" {
    var clk: FakeClock = .{};
    const r1 = try GraceRing.init(testing.allocator, .{ .cap = 8, .routine_bound_ms = 76_000, .pressure_bound_ms = 38_000, .clock = clk.clock() });
    defer r1.deinit();
    const r2 = try GraceRing.init(testing.allocator, .{ .cap = 8, .routine_bound_ms = 76_000, .pressure_bound_ms = 38_000, .clock = clk.clock() });
    defer r2.deinit();
    try testing.expect(!r1.defer_(5, 4096)); // unarmed: one relaxed load, false
    r1.arm();
    r2.arm();
    r1.memberJoin(1);
    r1.memberJoin(2);
    try testing.expectEqual(@as(u64, 0), r1.publishedBound()); // nobody acked yet
    try testing.expect(r1.defer_(5, 4096));
    clk.now = 2000;
    try testing.expect(r1.defer_(6, 4096));
    try testing.expect(r2.defer_(6, 4096));
    try testing.expectEqual(@as(u64, 2), r1.length());
    try testing.expect(r1.holds(5) and r1.holds(6) and !r1.holds(7));
    var out: [8]u64 = undefined;
    try testing.expectEqual(@as(usize, 0), r1.harvest(8, &out)); // bound 0 < label 1000
    // Member 1 acks 2000, member 2 only 1000: the bound is the MIN.
    r1.noteAck(1, 2000);
    r1.noteAck(2, 1000);
    try testing.expectEqual(@as(u64, 1000), r1.publishedBound());
    try testing.expectEqual(@as(usize, 1), r1.harvest(8, &out));
    try testing.expectEqual(@as(u64, 5), out[0]);
    try testing.expect(r1.closureHolds());
    r1.noteAck(2, 2000);
    try testing.expectEqual(@as(usize, 1), r1.harvest(8, &out));
    try testing.expectEqual(@as(u64, 6), out[0]);
    try testing.expectEqual(@as(u64, 0), r1.length());
    try testing.expect(r1.closureHolds());
    // r2 has no members: the bound is maxInt — nobody can hold a stale binding.
    try testing.expectEqual(std.math.maxInt(u64), r2.publishedBound());
    try testing.expectEqual(@as(usize, 1), r2.harvest(8, &out));
    try testing.expect(r2.closureHolds());
    try testing.expectEqual(@as(u64, 2), r1.deferrals.load(.monotonic));
    try testing.expectEqual(@as(u64, 1), r2.deferrals.load(.monotonic));
    // A member leaving drops out of the bound.
    r1.memberJoin(3);
    clk.now = 3000;
    try testing.expect(r1.defer_(9, 4096));
    try testing.expectEqual(@as(usize, 0), r1.harvest(8, &out)); // member 3 never acked
    r1.memberLeave(3);
    r1.noteAck(1, 3000);
    r1.noteAck(2, 3000);
    try testing.expectEqual(@as(usize, 1), r1.harvest(8, &out));
}

test "the laggard is fenced, not waited on: past the deadline (or over cap) force_progress evicts every member below the oldest label, recomputes the bound and the releases are counted forced" {
    var clk: FakeClock = .{};
    var fl: FenceLog = .{};
    const r = try GraceRing.init(testing.allocator, .{ .cap = 4, .routine_bound_ms = 76_000, .pressure_bound_ms = 38_000, .clock = clk.clock(), .fence = .{ .ctx = &fl, .fence = &FenceLog.fence } });
    defer r.deinit();
    r.arm();
    r.memberJoin(1);
    r.memberJoin(2);
    try testing.expect(r.defer_(10, 4096));
    r.noteAck(1, 1000);
    var out: [8]u64 = undefined;
    // Below the deadline: nothing moves.
    clk.now = 1000 + 76_000 - 1;
    try testing.expectEqual(@as(usize, 0), r.harvest(8, &out));
    try testing.expectEqual(@as(usize, 0), fl.n);
    // At the routine deadline the laggard (member 2, never acked) is fenced.
    const forced0 = stats.free_grace_forced_releases.load();
    clk.now = 1000 + 76_000;
    try testing.expectEqual(@as(usize, 1), r.harvest(8, &out));
    try testing.expectEqual(@as(u64, 10), out[0]);
    try testing.expectEqual(@as(usize, 1), fl.n);
    try testing.expectEqual(@as(u32, 2), fl.fenced[0]);
    try testing.expectEqual(forced0 + 1, stats.free_grace_forced_releases.load());
    try testing.expectEqual(@as(u64, 1), r.timeout_deferrals.load(.monotonic));
    try testing.expect(r.closureHolds());
    // Over cap forces at once whatever the clock says; a full ring refuses
    // a further deferral (the caller publishes).
    r.memberJoin(3);
    var i: u64 = 0;
    while (i < 4) : (i += 1) try testing.expect(r.defer_(20 + i, 4096));
    try testing.expect(!r.defer_(30, 4096));
    try testing.expectEqual(@as(u64, 4), r.length());
    try testing.expectEqual(@as(usize, 2), r.harvest(2, &out));
    // Both live members' acks are below the new label: both are laggards.
    try testing.expectEqual(@as(usize, 3), fl.n);
    try testing.expectEqual(@as(u32, 1), fl.fenced[1]);
    try testing.expectEqual(@as(u32, 3), fl.fenced[2]);
    try testing.expectEqual(@as(u64, 2), r.length());
    try testing.expect(r.closureHolds());
    // The valve: a fuller ring slides the deadline toward the pressure bound.
    try testing.expect(r.oldestLabel().? == clk.now);
}

test "under GPFS-strict tokens the ring is the TIMEOUT path alone: a gated recall publishes directly; a live member's unacked recall opens a T_owner window during which frees ride the ring" {
    var clk: FakeClock = .{};
    const Gate = struct {
        var verdict: RecallVerdict = .gated;
        fn v(_: ?*anyopaque) RecallVerdict {
            return verdict;
        }
    };
    Gate.verdict = .gated;
    const r = try GraceRing.init(testing.allocator, .{ .cap = 8, .routine_bound_ms = 76_000, .pressure_bound_ms = 38_000, .clock = clk.clock(), .recall_gate = .{ .ctx = null, .verdict = &Gate.v } });
    defer r.deinit();
    r.arm();
    r.memberJoin(1);
    const gated0 = stats.free_grace_recall_gated_frees.load();
    try testing.expect(!r.defer_(1, 4096));
    try testing.expectEqual(gated0 + 1, stats.free_grace_recall_gated_frees.load());
    // An unacked live recall opens the window: frees ride the ring for T_owner.
    r.noteRecallUnackedLive();
    const to0 = stats.free_grace_recall_timeout_deferrals.load();
    try testing.expect(r.defer_(2, 4096));
    try testing.expectEqual(to0 + 1, stats.free_grace_recall_timeout_deferrals.load());
    clk.now = 1000 + T_OWNER_MS;
    try testing.expect(!r.defer_(3, 4096)); // the window closed: gated again
    Gate.verdict = .open;
    try testing.expect(r.defer_(4, 4096));
    try testing.expect(r.closureHolds());
}

test "the allocator seam: a terminal free on an armed ring defers, the funnel head harvests it back once acked; the ENOSPC fence-bound source reads the routine bound" {
    var clk: FakeClock = .{};
    const r = try GraceRing.init(testing.allocator, .{ .cap = 8, .routine_bound_ms = 76_000, .pressure_bound_ms = 38_000, .clock = clk.clock() });
    defer r.deinit();
    const a = try block_alloc.BlockAllocator.init(testing.allocator, .{ .vol_tag = 1, .vol = 0, .block_size = 4096, .capacity_bytes = 8 * 4096 });
    defer a.deinit();
    a.grace = r.allocatorSeam();
    _ = try a.allocateBlock();
    _ = try a.allocateBlock();
    r.arm();
    r.memberJoin(1);
    try testing.expectEqual(block_alloc.BeginFree.terminal, try a.freeBlock(0));
    try testing.expectEqual(@as(u64, 1), r.length());
    try testing.expectEqual(block_alloc.State.limbo, a.stateOf(0)); // held by the ring, not reallocatable
    try testing.expect(a.freshSupplyVisible());
    try testing.expectEqual(@as(u64, 2), (try a.allocateBlock()).idx); // the funnel never hands out a graced block
    r.noteAck(1, 1000);
    try testing.expectEqual(@as(u64, 0), (try a.allocateBlock()).idx); // harvested at the funnel head
    try testing.expectEqual(@as(u64, 0), r.length());
    try testing.expect(r.closureHolds());
    try testing.expectEqual(@as(u64, 76_000), enospc.fenceBoundBaseMs(r.fenceBoundSource()));
    try testing.expectEqual(@as(u64, 152_000), enospc.pressureParkWallMs(enospc.fenceBoundBaseMs(r.fenceBoundSource())));
}

test "the knob faces resolve explicit > derived and refuse a max below one cycle" {
    var env: std.process.Environ.Map = .init(testing.allocator);
    defer env.deinit();
    var clk: FakeClock = .{};
    const k0 = Knobs.fromEnv(&env);
    try testing.expect(k0.max_ms == null and k0.max_offsets == null and k0.valve);
    const c0 = try k0.resolve(1 << 30, 38_000, clk.clock());
    try testing.expectEqual(RING_CAP_FLOOR, c0.cap);
    try testing.expectEqual(@as(u64, 76_000), c0.routine_bound_ms);
    try env.put("SQUEEZEFS_FREE_GRACE_MAX_MS", "40000");
    try env.put("SQUEEZEFS_FREE_GRACE_MAX_OFFSETS", "4096");
    try env.put("SQUEEZEFS_FREE_GRACE_VALVE", "0");
    const k1 = Knobs.fromEnv(&env);
    const c1 = try k1.resolve(1 << 30, 38_000, clk.clock());
    try testing.expectEqual(@as(u64, 4096), c1.cap);
    try testing.expectEqual(@as(u64, 40_000), c1.routine_bound_ms);
    try testing.expect(!c1.valve);
    try env.put("SQUEEZEFS_FREE_GRACE_MAX_MS", "1000");
    try testing.expectError(error.BelowOneCycle, Knobs.fromEnv(&env).resolve(1 << 30, 38_000, clk.clock()));
}
