//! The router (charter §5.4 "Routing"; §6.1 "Striped … health- and fill-
//! weighted placement … a published immutable choice table"; `docs/porting/
//! data_layouts.md` §5). PORTABLE: the volume table, the health math, the
//! placement table and its latch-free pick, the mint volume / mint rotor
//! over `inos.zig`'s lane arithmetic.
//!
//! Volume ids are `vol-{16 hex}` — a random u64 drawn once, never reused;
//! `vol_tag` IS that u64 verbatim (the refs key component, `key.volumeTag`),
//! never an ordinal or a registration-order alias. Lifecycle as it touches
//! routing: `active → draining → retired` (+ `draining → active` on undrain,
//! `disabled`), `placement_eligible = healthy ∧ (recordless ∨ active)`; the
//! add/remove/drain verbs are rung 8's.
//!
//! Health math (integer, parts-per-million fills): `fill_health = (1 − fill)
//! × 1000`; `penalty = clamp(1000 × (fill − mean), 0, 300)` — 10 pp of fill
//! above the set mean costs 100 points, and balance never outvotes health (a
//! degraded volume still loses to a merely full one); `weight = health −
//! penalty` iff eligible else 0; the band = eligible rows with `weight ≥ max
//! × 9/10`; `pick` = one ArcSwap load + a round-robin scan of the band where
//! the fail-stop latch's instant gate has the last word. The table is
//! rebuilt on the health-worker cadence and on every registration / state /
//! health transition — NEVER per write.
//!
//! The mint half: `pick_mint_volume` = the healthy volumes within 9/10 of the
//! healthiest, round-robin; within a volume `MINT_SPREAD = STAMP_MAX_CURSORS
//! / 4 = 64` rotor slots (a freshly spread volume consumes ≤ ¼ of its stamp-
//! cursor budget, leaving ¾ for cursors travelling in with migrated slots),
//! so a volume's load is divisible into ≥ 64 movable slices from birth; the
//! ino is `makeGlobalIno(local, slot, W)` over the frozen `W = 2^16`.

const std = @import("std");
const fuzzkit = @import("../core/fuzzkit.zig");
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const derive = @import("../core/derive.zig");
const log = @import("../core/log.zig");
const key = @import("../meta/key.zig");
const inos = @import("../meta/inos.zig");
const ledger = @import("../meta/ledger.zig");
const record = @import("../meta/record.zig");
const dlm = @import("../meta/dlm.zig");
const arc_swap = @import("../sync/arc_swap.zig");
const jc = @import("../meta/journal_core.zig");

pub const stats = struct {
    /// `backend_placement_picks`: striped placements taken from the table.
    pub var backend_placement_picks: counters.ShardedCounter = .{};
    /// `placement_table_refreshes`: table rebuilds (cadence + transitions).
    pub var placement_table_refreshes: counters.ShardedCounter = .{};
    /// `placement_no_healthy_refusals`: a pick found the band empty — "No
    /// healthy storage backends available for write".
    pub var placement_no_healthy_refusals: counters.ShardedCounter = .{};
    /// `backend_fill_spread`: `max − min` fill over the eligible rows, ppm
    /// (≤ 2 × `rebalance.REBALANCE_BAND_PPM` after a rebalance: every row
    /// within 10 pp of the mean — the planner raises the under-filled rows
    /// to the band's floor and never pushes a source below the mean).
    pub var backend_fill_spread: counters.Gauge = .{};
    /// `placement_rotor_slots`: rotor slots in the mint set of the volume
    /// last spread (the placement rotor — the fallback of a volume whose
    /// slot-lease plane is unarmed; `slot_rotor` is the plane's `M`).
    pub var placement_rotor_slots: counters.Gauge = .{};
    /// `placement_rotor_mints`: inos minted through the placement rotor
    /// (the plane's decision counts `rotor_mints` / `affinity_mints`).
    pub var placement_rotor_mints: counters.ShardedCounter = .{};
    /// `mint_volume_picks`: `pick_mint_volume` decisions.
    pub var mint_volume_picks: counters.ShardedCounter = .{};
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"backend_placement_picks\":{d},\"placement_table_refreshes\":{d},\"placement_no_healthy_refusals\":{d},\"backend_fill_spread\":{d},\"placement_rotor_slots\":{d},\"placement_rotor_mints\":{d},\"mint_volume_picks\":{d}}}", .{
        stats.backend_placement_picks.load(),
        stats.placement_table_refreshes.load(),
        stats.placement_no_healthy_refusals.load(),
        stats.backend_fill_spread.load(),
        stats.placement_rotor_slots.load(),
        stats.placement_rotor_mints.load(),
        stats.mint_volume_picks.load(),
    });
}

// ---------------------------------------------------------------------------
// §5.1 Volume identity and lifecycle.
// ---------------------------------------------------------------------------

/// `vol-{16 lowercase hex}`: the tag IS the id.
pub const VolumeId = struct {
    tag: u64,

    pub const text_len: usize = "vol-".len + 16;

    /// A fresh id from a random draw (`Io.randomSecure` at the caller).
    pub fn mint(random: u64) VolumeId {
        return .{ .tag = random };
    }

    pub fn format(self: VolumeId, w: *std.Io.Writer) std.Io.Writer.Error!void {
        try w.print("vol-{x:0>16}", .{self.tag});
    }

    pub fn text(self: VolumeId, buf: *[text_len]u8) []const u8 {
        return std.fmt.bufPrint(buf, "{f}", .{self}) catch unreachable;
    }

    /// Strict: exactly `vol-` + 16 hex digits (the tag the key codec
    /// derives for the same string — one derivation, `key.volumeTag`).
    pub fn parse(s: []const u8) ?VolumeId {
        if (s.len != text_len or !std.mem.startsWith(u8, s, "vol-")) return null;
        for (s[4..]) |c| _ = std.fmt.charToDigit(c, 16) catch return null;
        return .{ .tag = key.volumeTag(s) };
    }
};

pub const VolumeState = enum(u8) {
    active,
    draining,
    retired,
    disabled,

    /// The lattice: `active ↔ draining → retired`; `active ↔ disabled`;
    /// retired is terminal (its id is never reused).
    pub fn canTransition(from: VolumeState, to: VolumeState) bool {
        if (from == to) return false;
        return switch (from) {
            .active => to == .draining or to == .disabled,
            .draining => to == .active or to == .retired,
            .disabled => to == .active,
            .retired => false,
        };
    }
};

pub const Volume = struct {
    id: VolumeId,
    state: VolumeState = .active,
    /// A volume with no durable lifecycle record (a pre-lifecycle set) is
    /// eligible by its health alone.
    recordless: bool = false,
    capacity_bytes: u64,
    used_bytes: u64 = 0,
    /// The fail-stop latch / node-probe verdict: read INSTANTLY by every
    /// pick, even against a stale table. release/acquire pair with the
    /// marking site.
    healthy: std.atomic.Value(bool) = .init(true),

    pub fn isHealthy(self: *const Volume) bool {
        return self.healthy.load(.acquire);
    }

    /// `healthy ∧ (recordless ∨ state == active)`.
    pub fn placementEligible(self: *const Volume) bool {
        return self.isHealthy() and (self.recordless or self.state == .active);
    }

    pub fn fill(self: *const Volume) u64 {
        return fillPpm(self.used_bytes, self.capacity_bytes);
    }
};

// ---------------------------------------------------------------------------
// §5.2 Health math — integer, ppm.
// ---------------------------------------------------------------------------

pub const PPM: u64 = 1_000_000;
/// 10 pp of fill above the set mean costs 100 health points.
pub const BALANCE_PENALTY_K: u64 = 1000;
/// Balance never outvotes health: a degraded volume still loses to a merely
/// full one.
pub const BALANCE_PENALTY_CAP: u64 = 300;
/// The band: rows within 9/10 of the best weight — spread, not fairness
/// bookkeeping; the mint volumes use the same 9/10.
pub const BAND_NUM: u64 = 9;
pub const BAND_DEN: u64 = 10;

/// `used / capacity` in ppm; an empty capacity reads full.
pub fn fillPpm(used: u64, capacity: u64) u64 {
    if (capacity == 0) return PPM;
    const u: u128 = used;
    return @intCast(@min(u * PPM / capacity, PPM));
}

/// `(1 − fill) × 1000` — "fill health" (Q-17: the term carries no device
/// health yet; the fence/latch is the absolute gate).
pub fn fillHealth(fill_ppm: u64) u64 {
    return (PPM - @min(fill_ppm, PPM)) * 1000 / PPM;
}

/// `clamp(round(1000 × (fill − mean)), 0, 300)`.
pub fn balancePenalty(fill_ppm: u64, mean_ppm: u64) u64 {
    if (fill_ppm <= mean_ppm) return 0;
    const d = fill_ppm - mean_ppm;
    const points = (d * BALANCE_PENALTY_K + PPM / 2) / PPM;
    return @min(points, BALANCE_PENALTY_CAP);
}

pub fn healthEffective(health: u64, penalty: u64) u64 {
    return health -| penalty;
}

// ---------------------------------------------------------------------------
// The placement table — immutable per snapshot.
// ---------------------------------------------------------------------------

pub const PlacementRow = struct {
    tag: u64,
    fill_ppm: u64,
    /// `health_effective` iff eligible else 0.
    weight: u64,
    eligible: bool,
};

pub const PlacementTable = struct {
    rows: []PlacementRow,
    /// Indices of the band rows, in registration order.
    band: []u32,
    /// The round-robin cursor over the band.
    rr: std.atomic.Value(u64) = .init(0),
    fill_spread_ppm: u64,
    /// Eligible rows (the staleness probe compares populations).
    eligible_count: u32,

    /// Census: `set_mean = Σused / Σcap` over the ELIGIBLE rows; every row's
    /// weight; the band; the spread. Runs on the cadence, never per write.
    pub fn build(gpa: Allocator, volumes: []const Volume) Allocator.Error!PlacementTable {
        const rows = try gpa.alloc(PlacementRow, volumes.len);
        errdefer gpa.free(rows);
        var sum_used: u128 = 0;
        var sum_cap: u128 = 0;
        var eligible: u32 = 0;
        for (volumes) |*v| {
            if (!v.placementEligible()) continue;
            sum_used += v.used_bytes;
            sum_cap += v.capacity_bytes;
            eligible += 1;
        }
        const mean_ppm: u64 = if (sum_cap == 0) PPM else @intCast(@min(sum_used * PPM / sum_cap, PPM));
        var max_w: u64 = 0;
        var min_fill: u64 = PPM;
        var max_fill: u64 = 0;
        for (volumes, 0..) |*v, i| {
            const f = v.fill();
            const el = v.placementEligible();
            const w = if (el) healthEffective(fillHealth(f), balancePenalty(f, mean_ppm)) else 0;
            rows[i] = .{ .tag = v.id.tag, .fill_ppm = f, .weight = w, .eligible = el };
            if (el) {
                max_w = @max(max_w, w);
                min_fill = @min(min_fill, f);
                max_fill = @max(max_fill, f);
            }
        }
        // A zero max keeps every eligible row (all full: still spread).
        const floor = max_w * BAND_NUM / BAND_DEN;
        var band_len: usize = 0;
        for (rows) |r| {
            if (r.eligible and r.weight >= floor) band_len += 1;
        }
        const band = try gpa.alloc(u32, band_len);
        var n: usize = 0;
        for (rows, 0..) |r, i| {
            if (r.eligible and r.weight >= floor) {
                band[n] = @intCast(i);
                n += 1;
            }
        }
        return .{
            .rows = rows,
            .band = band,
            .fill_spread_ppm = if (eligible == 0) 0 else max_fill - min_fill,
            .eligible_count = eligible,
        };
    }

    pub fn deinit(self: *PlacementTable, gpa: Allocator) void {
        gpa.free(self.rows);
        gpa.free(self.band);
        self.* = undefined;
    }

    /// `start = rr.fetch_add(1)`; scan the band from `start`; the first row
    /// the instant health gate admits. Zero locks, zero syscalls.
    pub fn pick(self: *PlacementTable, gate: HealthGate) ?u64 {
        if (self.band.len == 0) return null;
        const start = self.rr.fetchAdd(1, .monotonic);
        for (0..self.band.len) |k| {
            const candidate = self.rows[self.band[@intCast((start + k) % self.band.len)]];
            if (gate.healthy(gate.ctx, candidate.tag)) {
                stats.backend_placement_picks.inc();
                return candidate.tag;
            }
        }
        return null;
    }

    pub fn row(self: *const PlacementTable, tag: u64) ?PlacementRow {
        for (self.rows) |r| if (r.tag == tag) return r;
        return null;
    }
};

/// The fail-stop latch's verdict for a tag, read at pick time.
pub const HealthGate = struct {
    ctx: *anyopaque,
    healthy: *const fn (ctx: *anyopaque, tag: u64) bool,
};

/// The published choice table: arc-swapped, readers latch-free.
pub const Placement = struct {
    table: arc_swap.ArcSwap(PlacementTable),

    const TableArc = arc_swap.Arc(PlacementTable);

    pub fn init(gpa: Allocator, volumes: []const Volume) Allocator.Error!Placement {
        const t = try PlacementTable.build(gpa, volumes);
        const a = TableArc.create(gpa, t) catch |e| {
            var tt = t;
            tt.deinit(gpa);
            return e;
        };
        stats.placement_table_refreshes.inc();
        stats.backend_fill_spread.set(t.fill_spread_ppm);
        return .{ .table = .init(a) };
    }

    pub fn deinit(self: *Placement) void {
        self.table.deinit();
    }

    /// Rebuild and publish; in-flight picks finish on the old snapshot.
    pub fn refresh(self: *Placement, gpa: Allocator, volumes: []const Volume) Allocator.Error!void {
        const t = try PlacementTable.build(gpa, volumes);
        const a = TableArc.create(gpa, t) catch |e| {
            var tt = t;
            tt.deinit(gpa);
            return e;
        };
        self.table.store(a);
        stats.placement_table_refreshes.inc();
        stats.backend_fill_spread.set(t.fill_spread_ppm);
    }

    /// ONE ArcSwap load + an O(band) scan.
    pub fn pick(self: *Placement, gate: HealthGate) ?u64 {
        const a = self.table.load();
        defer a.release();
        return a.value.pick(gate);
    }

    /// The current snapshot (stats, tests); release it.
    pub fn snapshot(self: *Placement) *TableArc {
        return self.table.load();
    }

    /// Is the snapshot built over this many eligible volumes?
    pub fn current(self: *Placement, eligible_now: u32) bool {
        const a = self.table.load();
        defer a.release();
        return a.value.eligible_count == eligible_now;
    }
};

// ---------------------------------------------------------------------------
// Stripe-unit placement (block_map_tree.md §8 Q8, option A).
// ---------------------------------------------------------------------------

/// A striped file's blocks are placed in STRIPE UNITS: `stripeUnitBlocks`
/// consecutive block indices of one file land on ONE volume (the unit's
/// index rotates the choice), so a sequential stream's inserts are offset-
/// consecutive on one volume under the allocator's "lowest clear run at/
/// above the frontier" carve and the merge emits RUN records straight from
/// its window — no post-hoc canonicalizer, no whole-map pass. Per-block
/// round-robin can never form a run on a set of ≥ 2 volumes.
///
/// `clamp(RUN_LEN_MAX / eligible, 16, 256)`: the ceiling keeps one unit at
/// 1/16 of a run's span, so the longest run a stream can emit stays far
/// inside the floor probe's bound while every volume still sees a share of
/// a stream shorter than 16 units; the floor of 16 blocks (64 MiB at 4 MiB)
/// keeps one stream's device parallelism across `eligible` volumes — the
/// prefetch pipeline's K = 4 offset lanes already parallelize ACROSS blocks,
/// so per-unit spread costs the read side nothing it had.
pub const STRIPE_UNIT_MIN_BLOCKS: u32 = 16;
pub const STRIPE_UNIT_MAX_BLOCKS: u32 = 256;

pub fn stripeUnitBlocks(eligible_volumes: u32) u32 {
    const share = record.RUN_LEN_MAX / @max(eligible_volumes, 1);
    return std.math.clamp(share, STRIPE_UNIT_MIN_BLOCKS, STRIPE_UNIT_MAX_BLOCKS);
}

/// The unit's volume among `eligible` (a dense ordinal into the caller's
/// eligible list): the file's hash spreads files, the unit index rotates
/// a file's units round-robin over the set.
pub fn stripeUnitChoice(ino: u64, block: u32, eligible_volumes: u32) u32 {
    assert(eligible_volumes != 0);
    const unit: u64 = block / stripeUnitBlocks(eligible_volumes);
    return @intCast((dlm.splitmix64(ino) +% unit) % eligible_volumes);
}

// ---------------------------------------------------------------------------
// §5.3 Metadata volume health, the mint volume, the mint rotor.
// ---------------------------------------------------------------------------

/// `min(1000, free / total × 1000)`; 0 when disabled — directory/file
/// placement by estimated remaining capacity.
pub fn volumeHealth(free_extents: u64, total_extents: u64, enabled: bool) u32 {
    if (!enabled or total_extents == 0) return 0;
    return @intCast(@min(1000, free_extents * 1000 / total_extents));
}

/// One volume ⇒ the parent's; else the ENABLED volumes within 9/10 of the
/// healthiest, round-robin on `rr` (per router — never a process static).
/// `healths[i] == null` = disabled.
pub fn pickMintVolume(parent_v: u32, healths: []const ?u32, rr: *std.atomic.Value(u64)) u32 {
    assert(healths.len >= 1);
    if (healths.len == 1) return 0;
    return pickMintVolumeAt(parent_v, healths, rr.fetchAdd(1, .monotonic));
}

/// `pickMintVolume` at a caller-chosen ORDINAL instead of the shared
/// round-robin word: the `ordinal mod |band|`-th member of the healthy
/// 9/10 band in index order. The directory placement rule (decision 0003
/// §BZ) passes `parent's member + the directories placed under the
/// parent`, so a parent's directory children walk the band from its own
/// member — a shared counter stepped by every pick made a tree whose
/// creation interleaves levels regularly (each directory, then its one
/// subdirectory) land every directory of one level on one member.
pub fn pickMintVolumeAt(parent_v: u32, healths: []const ?u32, ordinal: u64) u32 {
    assert(healths.len >= 1);
    if (healths.len == 1) return 0;
    var max: u32 = 0;
    var enabled: u32 = 0;
    for (healths) |h| if (h) |v| {
        max = @max(max, v);
        enabled += 1;
    };
    if (enabled == 0) return parent_v;
    const floor: u32 = @intCast(@as(u64, max) * BAND_NUM / BAND_DEN);
    var top: u32 = 0;
    for (healths) |h| if (h) |v| if (v >= floor) {
        top += 1;
    };
    const k = ordinal % top;
    var seen: u64 = 0;
    for (healths, 0..) |h, i| if (h) |v| if (v >= floor) {
        if (seen == k) {
            stats.mint_volume_picks.inc();
            return @intCast(i);
        }
        seen += 1;
    };
    unreachable;
}

/// `STAMP_MAX_CURSORS / 4`: a freshly spread volume consumes ≤ ¼ of its
/// stamp-cursor budget, leaving ¾ for cursors travelling in with migrated
/// slots.
pub const MINT_SPREAD: usize = ledger.STAMP_MAX_CURSORS / 4;

comptime {
    assert(MINT_SPREAD == 64);
}

/// Per volume: the MINT SET = its first `min(MINT_SPREAD, hosted)` hosted
/// slots ascending; `pick_mint_slot` rotates over it.
pub const MintRotor = struct {
    mints: [MINT_SPREAD]u16 = @splat(0),
    len: u8 = 0,
    rr: std.atomic.Value(u64) = .init(0),

    pub fn init(hosted_ascending: []const u16) MintRotor {
        var r: MintRotor = .{};
        const n = @min(MINT_SPREAD, hosted_ascending.len);
        for (hosted_ascending[0..n], 0..) |s, i| {
            if (i > 0) assert(s > hosted_ascending[i - 1]);
            r.mints[i] = s;
        }
        r.len = @intCast(n);
        stats.placement_rotor_slots.set(n);
        return r;
    }

    pub fn mintSet(self: *const MintRotor) []const u16 {
        return self.mints[0..self.len];
    }

    /// Width ≤ 1 ⇒ slot 0; else `mints[rr.fetch_add(1) % len]`.
    pub fn pickSlot(self: *MintRotor) u16 {
        if (self.len == 0) return 0;
        const k = self.rr.fetchAdd(1, .monotonic) % self.len;
        stats.placement_rotor_mints.inc();
        return self.mints[@intCast(k)];
    }
};

/// The ino a `(local, slot)` mint publishes: `(local − 2)·W + slot + 2`
/// over the frozen width (ino 1 = the root pin).
pub fn mintGlobalIno(local: u64, slot: u16) u64 {
    return inos.makeGlobalIno(local, slot, inos.ROUTING_WIDTH);
}

/// The slot a global ino routes to — the mint slot and the forest slot are
/// the same number by construction.
pub fn slotOf(global_ino: u64) u16 {
    return @intCast(inos.routeIno(global_ino, inos.ROUTING_WIDTH).slot);
}

// ---------------------------------------------------------------------------
// The volume table.
// ---------------------------------------------------------------------------

/// The data volumes one set can name: the placement scan is O(volumes) per
/// pick and the table is a fixed array published by count (no allocation
/// under a reader); 256 keeps a pick under a microsecond and is 16× the
/// largest set the reference ever ran.
pub const MAX_VOLUMES: usize = 256;

pub const RouterError = error{ VolumeTableFull, DuplicateVolume, UnknownVolume, InvalidTransition, OutOfMemory };

pub const Router = struct {
    gpa: Allocator,
    /// The format's block size: `const` after open (never a knob, Q-12).
    chunk: u64,
    volumes: [MAX_VOLUMES]Volume = undefined,
    /// Published with release after the slot is written; readers index
    /// below the acquire load.
    count: std.atomic.Value(u32) = .init(0),
    /// Control-plane mutations (add, state, census) serialize here; picks
    /// never take it.
    lock: jc.SpinLock = .{},
    placement: Placement,
    mint_rr: std.atomic.Value(u64) = .init(0),

    pub fn init(gpa: Allocator, chunk: u64) Allocator.Error!Router {
        return .{ .gpa = gpa, .chunk = chunk, .placement = try Placement.init(gpa, &.{}) };
    }

    pub fn deinit(self: *Router) void {
        self.placement.deinit();
        self.* = undefined;
    }

    pub fn volumesLive(self: *const Router) []const Volume {
        return self.volumes[0..self.count.load(.acquire)];
    }

    /// The registration: the row is written, published by count, and the
    /// table refreshed LAST (write-path selection enabled last).
    pub fn addVolume(self: *Router, v: Volume) RouterError!void {
        self.lock.lock();
        defer self.lock.unlock();
        const n = self.count.load(.acquire);
        for (self.volumes[0..n]) |*e| if (e.id.tag == v.id.tag) return error.DuplicateVolume;
        if (n == MAX_VOLUMES) return error.VolumeTableFull;
        self.volumes[n] = v;
        self.count.store(n + 1, .release);
        try self.refreshLocked();
    }

    fn findLocked(self: *Router, tag: u64) ?*Volume {
        for (self.volumes[0..self.count.load(.acquire)]) |*v| if (v.id.tag == tag) return v;
        return null;
    }

    /// `vol_tag → volume`, never an alias.
    pub fn volume(self: *Router, tag: u64) ?*const Volume {
        return self.findLocked(tag);
    }

    pub fn setState(self: *Router, tag: u64, to: VolumeState) RouterError!void {
        self.lock.lock();
        defer self.lock.unlock();
        const v = self.findLocked(tag) orelse return error.UnknownVolume;
        if (!VolumeState.canTransition(v.state, to)) return error.InvalidTransition;
        log.info("data volume {f}: {t} → {t}", .{ v.id, v.state, to });
        v.state = to;
        try self.refreshLocked();
    }

    /// The fail-stop latch: INSTANT for every pick (the gate reads the
    /// flag), the table follows on the next refresh.
    pub fn markUnhealthy(self: *Router, tag: u64) void {
        if (self.findLocked(tag)) |v| v.healthy.store(false, .release);
    }

    pub fn markHealthy(self: *Router, tag: u64) void {
        if (self.findLocked(tag)) |v| v.healthy.store(true, .release);
    }

    /// The allocator census feeding the fill term.
    pub fn updateCensus(self: *Router, tag: u64, used_bytes: u64) RouterError!void {
        self.lock.lock();
        defer self.lock.unlock();
        const v = self.findLocked(tag) orelse return error.UnknownVolume;
        v.used_bytes = used_bytes;
    }

    /// The health-worker cadence.
    pub fn refreshPlacement(self: *Router) Allocator.Error!void {
        self.lock.lock();
        defer self.lock.unlock();
        try self.refreshLocked();
    }

    fn refreshLocked(self: *Router) Allocator.Error!void {
        try self.placement.refresh(self.gpa, self.volumesLive());
    }

    fn gateHealthy(ctx: *anyopaque, tag: u64) bool {
        const self: *Router = @ptrCast(@alignCast(ctx));
        const v = self.findLocked(tag) orelse return false;
        return v.isHealthy();
    }

    pub fn gate(self: *Router) HealthGate {
        return .{ .ctx = self, .healthy = gateHealthy };
    }

    pub fn eligibleCount(self: *Router) u32 {
        var n: u32 = 0;
        for (self.volumesLive()) |*v| n += @intFromBool(v.placementEligible());
        return n;
    }

    /// `get_active_backend`: a pick off the snapshot; a stale snapshot (the
    /// eligible population moved) or an empty band rebuilds ONCE, then
    /// refuses "No healthy storage backends available for write" (counted).
    pub fn pickVolume(self: *Router) Allocator.Error!?u64 {
        if (self.placement.pick(self.gate())) |tag| return tag;
        try self.refreshPlacement();
        if (self.placement.pick(self.gate())) |tag| return tag;
        stats.placement_no_healthy_refusals.inc();
        return null;
    }

    /// `pick_mint_volume` over the metadata volumes' healths.
    pub fn mintVolume(self: *Router, parent_v: u32, healths: []const ?u32) u32 {
        return pickMintVolume(parent_v, healths, &self.mint_rr);
    }

    /// The per-row placement family for `.stats`.
    pub fn writePlacementJson(self: *Router, w: *std.Io.Writer) std.Io.Writer.Error!void {
        const a = self.placement.snapshot();
        defer a.release();
        try w.writeAll("{\"backend_placement\":{");
        for (a.value.rows, 0..) |r, i| {
            if (i != 0) try w.writeByte(',');
            try w.print("\"vol-{x:0>16}\":{{\"backend_placement_weight\":{d},\"backend_fill_ratio_ppm\":{d},\"eligible\":{}}}", .{ r.tag, r.weight, r.fill_ppm, r.eligible });
        }
        try w.print("}},\"backend_fill_spread\":{d},\"placement_band\":{d}}}", .{ a.value.fill_spread_ppm, a.value.band.len });
    }
};

// ---------------------------------------------------------------------------
// Tests — the ties, the laws, the table.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "volume ids: vol-{16 hex} formats, parses strictly, and the tag is the key codec's derivation verbatim" {
    const id = VolumeId.mint(0x0123_4567_89AB_CDEF);
    var buf: [VolumeId.text_len]u8 = undefined;
    try testing.expectEqualStrings("vol-0123456789abcdef", id.text(&buf));
    try testing.expectEqual(id, VolumeId.parse("vol-0123456789abcdef").?);
    try testing.expectEqual(id.tag, key.volumeTag("vol-0123456789abcdef"));
    try testing.expectEqual(VolumeId.mint(0), VolumeId.parse("vol-0000000000000000").?);
    try testing.expect(VolumeId.parse("vol-0123456789abcde") == null);
    try testing.expect(VolumeId.parse("vol-0123456789abcdefg") == null);
    try testing.expect(VolumeId.parse("vol-0123456789abcdeg") == null);
    try testing.expect(VolumeId.parse("nvme0n1") == null);
    try testing.expect(VolumeId.parse("vol-+123456789abcdef") == null);
}

test "the lifecycle lattice as it touches routing: active ↔ draining → retired, active ↔ disabled, retired terminal; eligibility = healthy ∧ (recordless ∨ active)" {
    try testing.expect(VolumeState.canTransition(.active, .draining));
    try testing.expect(VolumeState.canTransition(.draining, .active));
    try testing.expect(VolumeState.canTransition(.draining, .retired));
    try testing.expect(VolumeState.canTransition(.active, .disabled));
    try testing.expect(VolumeState.canTransition(.disabled, .active));
    try testing.expect(!VolumeState.canTransition(.active, .retired));
    try testing.expect(!VolumeState.canTransition(.retired, .active));
    try testing.expect(!VolumeState.canTransition(.retired, .draining));
    try testing.expect(!VolumeState.canTransition(.active, .active));
    var v: Volume = .{ .id = VolumeId.mint(1), .capacity_bytes = 100 };
    try testing.expect(v.placementEligible());
    v.state = .draining;
    try testing.expect(!v.placementEligible());
    v.recordless = true;
    try testing.expect(v.placementEligible());
    v.healthy.store(false, .release);
    try testing.expect(!v.placementEligible());
}

test "health math ties: fill_health = (1 − fill) × 1000; penalty slope 10 pp = 100 points, cap 300, zero at or below the mean, monotone in fill; balance never outvotes health" {
    try testing.expectEqual(@as(u64, 1000), fillHealth(0));
    try testing.expectEqual(@as(u64, 500), fillHealth(PPM / 2));
    try testing.expectEqual(@as(u64, 0), fillHealth(PPM));
    try testing.expectEqual(@as(u64, 100), fillHealth(900_000));
    try testing.expectEqual(@as(u64, 0), balancePenalty(300_000, 300_000));
    try testing.expectEqual(@as(u64, 0), balancePenalty(100_000, 300_000));
    try testing.expectEqual(@as(u64, 100), balancePenalty(400_000, 300_000));
    try testing.expectEqual(@as(u64, 50), balancePenalty(350_000, 300_000));
    try testing.expectEqual(@as(u64, 300), balancePenalty(600_000, 300_000));
    try testing.expectEqual(@as(u64, 300), balancePenalty(PPM, 0));
    try testing.expectEqual(@as(u64, 1), balancePenalty(500, 0));
    try testing.expectEqual(@as(u64, 0), balancePenalty(499, 0));
    var prng = std.Random.DefaultPrng.init(7);
    const r = prng.random();
    for (0..2000) |_| {
        const mean = r.intRangeAtMost(u64, 0, PPM);
        const a = r.intRangeAtMost(u64, 0, PPM);
        const b = r.intRangeAtMost(u64, a, PPM);
        try testing.expect(balancePenalty(a, mean) <= balancePenalty(b, mean));
        try testing.expect(balancePenalty(b, mean) <= BALANCE_PENALTY_CAP);
        // Never outvotes health: a volume 300+ points healthier than another
        // stays ahead whatever the fills.
        const h_degraded: u64 = 300;
        const h_full: u64 = 700;
        try testing.expect(healthEffective(h_full, balancePenalty(b, mean)) >= healthEffective(h_degraded, 0));
    }
    try testing.expectEqual(@as(u64, 0), healthEffective(100, 300));
    try testing.expectEqual(PPM, fillPpm(1, 0));
    try testing.expectEqual(@as(u64, 250_000), fillPpm(1 * derive.GiB, 4 * derive.GiB));
    try testing.expectEqual(PPM, fillPpm(5, 4));
}

const Fake = struct {
    unhealthy: u64 = 0,

    fn gate(self: *Fake) HealthGate {
        return .{ .ctx = self, .healthy = healthy };
    }

    fn healthy(ctx: *anyopaque, tag: u64) bool {
        const self: *Fake = @ptrCast(@alignCast(ctx));
        return tag != self.unhealthy;
    }
};

fn vol(tag: u64, cap: u64, used: u64) Volume {
    return .{ .id = VolumeId.mint(tag), .capacity_bytes = cap, .used_bytes = used };
}

test "the placement table over an imbalanced set: weights, the 9/10 band, round-robin over the band only, the fill spread; a zero max keeps every eligible row; ineligible rows weigh 0 and never enter the band" {
    const gpa = testing.allocator;
    // Four volumes: 10 %, 20 %, 30 %, 90 % full (equal capacity). Mean 37.5 %.
    const vols = [_]Volume{ vol(1, 1000, 100), vol(2, 1000, 200), vol(3, 1000, 300), vol(4, 1000, 900) };
    var t = try PlacementTable.build(gpa, &vols);
    defer t.deinit(gpa);
    try testing.expectEqual(@as(u64, 900), t.rows[0].weight);
    try testing.expectEqual(@as(u64, 800), t.rows[1].weight);
    try testing.expectEqual(@as(u64, 700), t.rows[2].weight);
    // 90 %: health 100, penalty clamp(1000 × 0.525) = 300 ⇒ 0.
    try testing.expectEqual(@as(u64, 0), t.rows[3].weight);
    try testing.expectEqual(@as(u64, 800_000), t.fill_spread_ppm);
    try testing.expectEqual(@as(u32, 4), t.eligible_count);
    // Band: weight ≥ 810 ⇒ only volume 1.
    try testing.expectEqual(@as(usize, 1), t.band.len);
    try testing.expectEqual(@as(u32, 0), t.band[0]);
    var fake: Fake = .{};
    const p0 = stats.backend_placement_picks.load();
    try testing.expectEqual(@as(?u64, 1), t.pick(fake.gate()));
    try testing.expectEqual(@as(?u64, 1), t.pick(fake.gate()));
    try testing.expectEqual(p0 + 2, stats.backend_placement_picks.load());
    // A balanced set: every row in the band, round-robin across them.
    const even = [_]Volume{ vol(1, 1000, 100), vol(2, 1000, 110), vol(3, 1000, 120) };
    var te = try PlacementTable.build(gpa, &even);
    defer te.deinit(gpa);
    try testing.expectEqual(@as(usize, 3), te.band.len);
    try testing.expectEqual(@as(u64, 20_000), te.fill_spread_ppm);
    var seen: [4]u32 = @splat(0);
    for (0..9) |_| seen[@intCast(te.pick(fake.gate()).?)] += 1;
    try testing.expectEqual(@as(u32, 3), seen[1]);
    try testing.expectEqual(@as(u32, 3), seen[2]);
    try testing.expectEqual(@as(u32, 3), seen[3]);
    // The instant gate skips an unhealthy row inside the band.
    fake.unhealthy = 2;
    for (0..6) |_| try testing.expect(te.pick(fake.gate()).? != 2);
    // Everything unhealthy at the gate ⇒ null.
    var all_bad: Fake = .{};
    const one = [_]Volume{vol(9, 1000, 0)};
    var t1 = try PlacementTable.build(gpa, &one);
    defer t1.deinit(gpa);
    all_bad.unhealthy = 9;
    try testing.expectEqual(@as(?u64, null), t1.pick(all_bad.gate()));
    // All full ⇒ max weight 0 ⇒ every eligible row kept.
    const full = [_]Volume{ vol(1, 10, 10), vol(2, 10, 10) };
    var tf = try PlacementTable.build(gpa, &full);
    defer tf.deinit(gpa);
    try testing.expectEqual(@as(usize, 2), tf.band.len);
    // Draining and unhealthy rows: weight 0, outside the band, outside the mean.
    var mixed = [_]Volume{ vol(1, 1000, 500), vol(2, 1000, 0), vol(3, 1000, 0) };
    mixed[1].state = .draining;
    mixed[2].healthy.store(false, .release);
    var tm = try PlacementTable.build(gpa, &mixed);
    defer tm.deinit(gpa);
    try testing.expectEqual(@as(u64, 0), tm.rows[1].weight);
    try testing.expectEqual(@as(u64, 0), tm.rows[2].weight);
    try testing.expectEqual(@as(u64, 500), tm.rows[0].weight);
    try testing.expectEqual(@as(usize, 1), tm.band.len);
    try testing.expectEqual(@as(u32, 1), tm.eligible_count);
    try testing.expectEqual(@as(u64, 0), tm.fill_spread_ppm);
    const empty = try PlacementTable.build(gpa, &.{});
    var e = empty;
    defer e.deinit(gpa);
    try testing.expectEqual(@as(?u64, null), e.pick(fake.gate()));
}

test "the router: registration publishes the table LAST; picks are table-only; an unhealthy mark is instant against a stale table; a state change refreshes eligibility; the refresh counter and the spread gauge track rebuilds; an empty band rebuilds once then refuses counted" {
    const gpa = testing.allocator;
    var r = try Router.init(gpa, 4 * derive.MiB);
    defer r.deinit();
    const ref0 = stats.placement_table_refreshes.load();
    try r.addVolume(vol(0xA, 1000, 100));
    try r.addVolume(vol(0xB, 1000, 100));
    try testing.expectError(error.DuplicateVolume, r.addVolume(vol(0xA, 1000, 0)));
    try testing.expectEqual(ref0 + 2, stats.placement_table_refreshes.load());
    try testing.expectEqual(@as(u64, 0), stats.backend_fill_spread.load());
    try testing.expectEqual(@as(usize, 2), r.volumesLive().len);
    try testing.expect(r.volume(0xA) != null);
    try testing.expect(r.volume(0xC) == null);
    // Round-robin across both.
    var a: u32 = 0;
    var b: u32 = 0;
    for (0..10) |_| {
        const tag = (try r.pickVolume()).?;
        if (tag == 0xA) a += 1 else b += 1;
    }
    try testing.expectEqual(@as(u32, 5), a);
    try testing.expectEqual(@as(u32, 5), b);
    // The census changes nothing until a refresh (picks are table-only).
    try r.updateCensus(0xB, 950);
    try testing.expectEqual(ref0 + 2, stats.placement_table_refreshes.load());
    for (0..4) |_| _ = try r.pickVolume();
    try r.refreshPlacement();
    try testing.expectEqual(ref0 + 3, stats.placement_table_refreshes.load());
    try testing.expectEqual(@as(u64, 850_000), stats.backend_fill_spread.load());
    for (0..6) |_| try testing.expectEqual(@as(?u64, 0xA), try r.pickVolume());
    // Instant unhealthy: the table still lists A in the band; the gate refuses it;
    // the empty band rebuilds once and B (weight 0 but the only eligible row) serves.
    r.markUnhealthy(0xA);
    try testing.expectEqual(@as(?u64, 0xB), try r.pickVolume());
    try testing.expectEqual(ref0 + 4, stats.placement_table_refreshes.load());
    r.markUnhealthy(0xB);
    const no0 = stats.placement_no_healthy_refusals.load();
    try testing.expectEqual(@as(?u64, null), try r.pickVolume());
    try testing.expectEqual(no0 + 1, stats.placement_no_healthy_refusals.load());
    r.markHealthy(0xA);
    r.markHealthy(0xB);
    // Draining leaves the band at the next refresh; undrain returns it.
    try r.setState(0xA, .draining);
    try testing.expectEqual(@as(u32, 1), r.eligibleCount());
    for (0..3) |_| try testing.expectEqual(@as(?u64, 0xB), try r.pickVolume());
    try testing.expectError(error.InvalidTransition, r.setState(0xA, .disabled));
    try r.setState(0xA, .active);
    try testing.expectEqual(@as(u32, 2), r.eligibleCount());
    try testing.expectError(error.UnknownVolume, r.setState(0xC, .draining));
    // The stats face.
    var buf: [1024]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try r.writePlacementJson(&w);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"vol-000000000000000a\":{\"backend_placement_weight\":") != null);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"placement_band\":") != null);
    w = .fixed(&buf);
    try writeStatsJson(&w);
    try testing.expect(std.mem.startsWith(u8, w.buffered(), "{\"backend_placement_picks\":"));
    try testing.expect(std.mem.endsWith(u8, w.buffered(), "}"));
}

test "stripe-unit tie: clamp(RUN_LEN_MAX / eligible, 16, 256) — 256 up to 16 volumes, 64 at 64, the 16-block floor from 256 volumes on; a file's consecutive blocks share one volume for a whole unit and the units rotate over the set; two files spread" {
    try testing.expectEqual(@as(u32, 256), stripeUnitBlocks(1));
    try testing.expectEqual(@as(u32, 256), stripeUnitBlocks(2));
    try testing.expectEqual(@as(u32, 256), stripeUnitBlocks(16));
    try testing.expectEqual(@as(u32, 128), stripeUnitBlocks(32));
    try testing.expectEqual(@as(u32, 64), stripeUnitBlocks(64));
    try testing.expectEqual(@as(u32, 16), stripeUnitBlocks(256));
    try testing.expectEqual(@as(u32, 16), stripeUnitBlocks(1000));
    try testing.expectEqual(@as(u32, 256), stripeUnitBlocks(0));
    // Every block of unit 0 of ino 7 on one volume; unit 1 on the next.
    const n: u32 = 2;
    const k = stripeUnitBlocks(n);
    const first = stripeUnitChoice(7, 0, n);
    var b: u32 = 0;
    while (b < k) : (b += 1) try testing.expectEqual(first, stripeUnitChoice(7, b, n));
    try testing.expectEqual((first + 1) % n, stripeUnitChoice(7, k, n));
    try testing.expectEqual(first, stripeUnitChoice(7, 2 * k, n));
    // Over 64 files, both volumes take unit 0 of some.
    var seen: [2]u32 = @splat(0);
    var ino: u64 = 2;
    while (ino < 66) : (ino += 1) seen[stripeUnitChoice(ino, 0, n)] += 1;
    try testing.expect(seen[0] > 0 and seen[1] > 0);
}

test "mint ties: W = 2^16; MINT_SPREAD = STAMP_MAX_CURSORS / 4 = 64; volume health = free/total × 1000 (0 disabled); pick_mint_volume keeps the 9/10 band and round-robins it; the rotor rotates over the first 64 hosted slots and the ino round-trips through the lane arithmetic" {
    try testing.expectEqual(@as(u64, 65_536), inos.ROUTING_WIDTH);
    try testing.expectEqual(@as(usize, 64), MINT_SPREAD);
    try testing.expectEqual(ledger.STAMP_MAX_CURSORS / 4, MINT_SPREAD);
    try testing.expectEqual(@as(u32, 1000), volumeHealth(10, 10, true));
    try testing.expectEqual(@as(u32, 250), volumeHealth(1, 4, true));
    try testing.expectEqual(@as(u32, 0), volumeHealth(10, 10, false));
    try testing.expectEqual(@as(u32, 0), volumeHealth(0, 0, true));
    var rr: std.atomic.Value(u64) = .init(0);
    // One volume: the parent's.
    try testing.expectEqual(@as(u32, 0), pickMintVolume(0, &.{900}, &rr));
    // Healths 900, 850, 700, disabled: the band is {900, 850} (≥ 810).
    const healths = [_]?u32{ 900, 850, 700, null };
    var seen: [4]u32 = @splat(0);
    for (0..8) |_| seen[pickMintVolume(2, &healths, &rr)] += 1;
    try testing.expectEqual(@as(u32, 4), seen[0]);
    try testing.expectEqual(@as(u32, 4), seen[1]);
    try testing.expectEqual(@as(u32, 0), seen[2]);
    try testing.expectEqual(@as(u32, 0), seen[3]);
    // All disabled: the parent's.
    try testing.expectEqual(@as(u32, 1), pickMintVolume(1, &.{ null, null }, &rr));
    // The rotor.
    var hosted: [100]u16 = undefined;
    for (&hosted, 0..) |*s, i| s.* = @intCast(i * 3 + 1);
    var rotor = MintRotor.init(&hosted);
    try testing.expectEqual(@as(usize, 64), rotor.mintSet().len);
    try testing.expectEqual(@as(u64, 64), stats.placement_rotor_slots.load());
    try testing.expectEqual(@as(u16, 1), rotor.mintSet()[0]);
    try testing.expectEqual(@as(u16, 190), rotor.mintSet()[63]);
    const m0 = stats.placement_rotor_mints.load();
    var hits: [200]u32 = @splat(0);
    for (0..128) |_| hits[rotor.pickSlot()] += 1;
    for (rotor.mintSet()) |s| try testing.expectEqual(@as(u32, 2), hits[s]);
    try testing.expectEqual(m0 + 128, stats.placement_rotor_mints.load());
    var small = MintRotor.init(&.{ 5, 9 });
    try testing.expectEqual(@as(u16, 5), small.pickSlot());
    try testing.expectEqual(@as(u16, 9), small.pickSlot());
    try testing.expectEqual(@as(u16, 5), small.pickSlot());
    var none = MintRotor.init(&.{});
    try testing.expectEqual(@as(u16, 0), none.pickSlot());
    // The ino: (local − 2)·W + slot + 2, inverse by routeIno.
    const ino = mintGlobalIno(7, 190);
    try testing.expectEqual((7 - 2) * 65_536 + 190 + 2, ino);
    try testing.expectEqual(@as(u16, 190), slotOf(ino));
    try testing.expectEqual(@as(u64, 7), inos.routeIno(ino, inos.ROUTING_WIDTH).local);
    try testing.expectEqual(@as(u64, 1), mintGlobalIno(1, 0));
    // The router owns its rotor counter (never a process static).
    var r = try Router.init(testing.allocator, 4 * derive.MiB);
    defer r.deinit();
    var seen2: [3]u32 = @splat(0);
    for (0..6) |_| seen2[r.mintVolume(0, &.{ 500, 500, 100 })] += 1;
    try testing.expectEqual(@as(u32, 3), seen2[0]);
    try testing.expectEqual(@as(u32, 3), seen2[1]);
}

fn volumeIdTextFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [64]u8 = undefined;
    const len = smith.slice(&buf);
    _ = VolumeId.parse(buf[0..len]);
}

test "fuzz: the volume-id text parser is total on arbitrary text" {
    try fuzzkit.fuzz({}, volumeIdTextFuzz, .{ .corpus = &.{ fuzzkit.slice("0"), fuzzkit.slice("65536"), fuzzkit.slice("-1") } });
}
