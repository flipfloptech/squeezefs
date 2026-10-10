//! Scan-resistant admission — R1b (charter §6.3 "Scan-resistant admission
//! (fills > 256 KiB): first-touch/streaming ⇒ hot-tier probation, disk
//! publish SKIPPED (the tier-publish tax: 16.9 GiB of device writes to
//! read 16 GiB once); ghost second touch (2^16-entry direct-mapped tag
//! table, two-epoch) ⇒ protected + validated NVMe-tier publish; waste
//! bounded `wasted_bytes ÷ device read bytes ≤ 5 %`"; `docs/porting/
//! data_read_path.md` §1.5, §6, §9, §13 Q11). PORTABLE.
//!
//! The measurement: reading 16 GiB cold cost 16.35 GiB of device reads
//! PLUS 16.9 GiB of tier WRITES — one 4 MiB memcpy + dirty-page writeback
//! per cold block, fighting the read stream for the same device; 44 % of
//! daemon CPU in memcpy. Skipping a publish is ALWAYS correctness-safe
//! (absence ⇒ the next reader goes to the device); the risky direction was
//! never "don't cache", it was "cache wrongly" — admission (don't insert)
//! beats eviction (insert then regret).
//!
//! Three instruments compose the decision: the GHOST (has this key been
//! missed before, within two sliding epochs of 2^15 misses — a single-epoch
//! match created a table-wide dead zone at every flip), the GOVERNOR (the
//! bounded-waste law: protected victims that served nothing are WASTE;
//! `wasted ≥ 2 blocks ∧ 2 × wasted ≥ evicted` CLAMPS admission to a trickle
//! of `fill_pct` % of the previous epoch's foreground device bytes, spent by
//! ONE token CAS — a check-then-add herd admitted ~6× the budget under
//! 256-deep O_DIRECT), and the COOLDOWN (a key re-escalates at most every
//! ~32–64 s: the ghost proves REUSE, the cooldown bounds CHURN). Every
//! epoch stamp is `clock.monotonicNs` (Q11: a wall clock steps).

const std = @import("std");
const fuzzkit = @import("../core/fuzzkit.zig");
const Allocator = std.mem.Allocator;
const Atomic = std.atomic.Value;

const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const knobs = @import("../core/knobs.zig");
const log = @import("../core/log.zig");
const rsc = @import("read_size_class.zig");
const hot_tier = @import("hot_tier.zig");

pub const BlockKey = rsc.BlockKey;
pub const PutClass = hot_tier.PutClass;

// ---------------------------------------------------------------------------
// Constants with their reasons.
// ---------------------------------------------------------------------------

/// `2^16` tag slots (~256 KiB once per router) and a two-epoch sliding
/// window of `2^15` misses: ≈ the table's half-population per epoch, so a
/// key's record survives one full table of other misses.
pub const GHOST_SLOTS: usize = 1 << 16;
pub const GHOST_EPOCH_MISSES: u64 = 1 << 15;
/// The cooldown epoch: ~32–64 s between re-escalations of one key — a
/// beyond-tier set degrades to the device-true ranged path between
/// windows instead of re-fetching + re-publishing 4 MiB per eviction.
pub const COOLDOWN_EPOCH_NS: u64 = 32 * std.time.ns_per_s;
/// The governor epoch: the house 2 s epoch.
pub const GOVERNOR_EPOCH_NS: u64 = 2 * std.time.ns_per_s;
/// The clamp's noise floor: `wasted ≥ 2 × block`.
pub const CLAMP_FLOOR_BLOCKS: u64 = 2;
/// Fibonacci hashing constant for the epoch tag mix.
const TAG_MIX: u32 = 0x9E3779B9;

pub const stats = struct {
    pub var read_fill_publishes_skipped: counters.ShardedCounter = .{};
    pub var read_tier_admissions: counters.ShardedCounter = .{};
    pub var read_tier_admission_ghost_hits: counters.ShardedCounter = .{};
    pub var read_admission_evicted_unhit: counters.ShardedCounter = .{};
    pub var read_admission_wasted_bytes: counters.ShardedCounter = .{};
    pub var read_admission_governor_denials: counters.ShardedCounter = .{};
    pub var read_admission_governor_clamped: counters.Gauge = .{};
    pub var read_admission_stream_transients: counters.ShardedCounter = .{};
    pub var ranged_read_ghost_escalations: counters.ShardedCounter = .{};
    pub var read_odirect_ghost_admits: counters.ShardedCounter = .{};
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"read_fill_publishes_skipped\":{d},\"read_tier_admissions\":{d},\"read_tier_admission_ghost_hits\":{d},\"read_admission_evicted_unhit\":{d},\"read_admission_wasted_bytes\":{d},\"read_admission_governor_denials\":{d},\"read_admission_governor_clamped\":{d},\"read_admission_stream_transients\":{d},\"ranged_read_ghost_escalations\":{d},\"read_odirect_ghost_admits\":{d}}}", .{
        stats.read_fill_publishes_skipped.load(),
        stats.read_tier_admissions.load(),
        stats.read_tier_admission_ghost_hits.load(),
        stats.read_admission_evicted_unhit.load(),
        stats.read_admission_wasted_bytes.load(),
        stats.read_admission_governor_denials.load(),
        stats.read_admission_governor_clamped.load(),
        stats.read_admission_stream_transients.load(),
        stats.ranged_read_ghost_escalations.load(),
        stats.read_odirect_ghost_admits.load(),
    });
}

/// `SQUEEZEFS_READ_TIER_ADMISSION`.
pub const Policy = enum {
    always,
    second_touch,
    never,

    pub fn parse(s: []const u8) Policy {
        if (std.mem.eql(u8, s, "always")) return .always;
        if (std.mem.eql(u8, s, "never")) return .never;
        return .second_touch;
    }

    /// The knob; a hot budget of 0 degrades `second-touch → always` loudly
    /// (no RAM tier can hold a first touch, so the disk tier must).
    pub fn fromEnv(env: *const std.process.Environ.Map, hot_budget_bytes: u64) Policy {
        const p = parse(knobs.enumKnob("SQUEEZEFS_READ_TIER_ADMISSION", env));
        if (hot_budget_bytes == 0 and p == .second_touch) {
            log.warn("read admission: the hot-block budget is 0, degrading SQUEEZEFS_READ_TIER_ADMISSION=second-touch to `always` (no RAM tier can hold a first touch)", .{});
            return .always;
        }
        return p;
    }
};

/// Why a fill is being fetched.
pub const FillClass = enum(u8) {
    demand,
    demand_stream,
    prefetch,
    /// A ranged second touch escalating to one whole-block fill: its
    /// publish is AWAITED (the admission IS the serve contract).
    escalation,

    pub fn streaming(self: FillClass) bool {
        return self == .demand_stream or self == .prefetch;
    }
};

// ---------------------------------------------------------------------------
// The ghost table.
// ---------------------------------------------------------------------------

pub const GhostTable = struct {
    slots: []Atomic(u32),
    epoch: Atomic(u32) = .init(1),
    misses: Atomic(u64) = .init(0),

    pub fn init(gpa: Allocator) Allocator.Error!GhostTable {
        const slots = try gpa.alloc(Atomic(u32), GHOST_SLOTS);
        @memset(slots, .init(0));
        return .{ .slots = slots };
    }

    pub fn deinit(self: *GhostTable, gpa: Allocator) void {
        gpa.free(self.slots);
        self.* = undefined;
    }

    /// `((hash >> 16) as u32) ^ (epoch × φ)`, never 0 (0 = empty).
    pub fn tag(hash: u64, epoch: u32) u32 {
        const t = @as(u32, @truncate(hash >> 16)) ^ (epoch *% TAG_MIX);
        return if (t == 0) 1 else t;
    }

    /// Hit iff the slot holds this key's tag for the current OR the previous
    /// epoch (the window slides one epoch at a time); the current tag is
    /// recorded regardless. A miss past `GHOST_EPOCH_MISSES` rolls the epoch
    /// (racy-tolerant: a double roll narrows one window).
    pub fn checkAndRecord(self: *GhostTable, key: BlockKey) bool {
        const h = key.hash();
        const slot = &self.slots[@intCast(h & (GHOST_SLOTS - 1))];
        const e = self.epoch.load(.monotonic);
        const seen = slot.load(.monotonic);
        const hit = seen == tag(h, e) or seen == tag(h, e -% 1);
        slot.store(tag(h, e), .monotonic);
        if (!hit) {
            if (self.misses.fetchAdd(1, .monotonic) + 1 >= GHOST_EPOCH_MISSES) {
                self.misses.store(0, .monotonic);
                _ = self.epoch.fetchAdd(1, .monotonic);
            }
        }
        return hit;
    }

    pub fn currentEpoch(self: *const GhostTable) u32 {
        return self.epoch.load(.monotonic);
    }
};

// ---------------------------------------------------------------------------
// The escalation cooldown (same shape, wall-time epochs on the monotonic clock).
// ---------------------------------------------------------------------------

pub const EscalationCooldown = struct {
    slots: []Atomic(u32),

    pub fn init(gpa: Allocator) Allocator.Error!EscalationCooldown {
        const slots = try gpa.alloc(Atomic(u32), GHOST_SLOTS);
        @memset(slots, .init(0));
        return .{ .slots = slots };
    }

    pub fn deinit(self: *EscalationCooldown, gpa: Allocator) void {
        gpa.free(self.slots);
        self.* = undefined;
    }

    fn epochOf(now_ns: u64) u32 {
        return @truncate(now_ns / COOLDOWN_EPOCH_NS + 1);
    }

    pub fn recentlyEscalated(self: *EscalationCooldown, key: BlockKey, now_ns: u64) bool {
        const h = key.hash();
        const seen = self.slots[@intCast(h & (GHOST_SLOTS - 1))].load(.monotonic);
        const e = epochOf(now_ns);
        return seen == GhostTable.tag(h, e) or seen == GhostTable.tag(h, e -% 1);
    }

    pub fn record(self: *EscalationCooldown, key: BlockKey, now_ns: u64) void {
        const h = key.hash();
        self.slots[@intCast(h & (GHOST_SLOTS - 1))].store(GhostTable.tag(h, epochOf(now_ns)), .monotonic);
    }
};

// ---------------------------------------------------------------------------
// The admission governor — the bounded-waste law.
// ---------------------------------------------------------------------------

pub const Governor = struct {
    /// `SQUEEZEFS_READ_ADMISSION_FILL_PCT` (0..=100).
    fill_pct: u64,
    epoch_ns: u64 = GOVERNOR_EPOCH_NS,
    epoch_idx: Atomic(u64) = .init(0),
    evicted_cur: Atomic(u64) = .init(0),
    evicted_prev: Atomic(u64) = .init(0),
    wasted_cur: Atomic(u64) = .init(0),
    wasted_prev: Atomic(u64) = .init(0),
    foreground_cur: Atomic(u64) = .init(0),
    foreground_prev: Atomic(u64) = .init(0),
    tokens: Atomic(u64) = .init(0),

    pub fn init(fill_pct: u64) Governor {
        std.debug.assert(fill_pct <= 100);
        return .{ .fill_pct = fill_pct };
    }

    /// The fill share and the epoch (`SQUEEZEFS_TEST_READ_ADMISSION_EPOCH_MS`:
    /// 0 = the 2 s house epoch).
    pub fn fromEnv(env: *const std.process.Environ.Map) Governor {
        var g = init(knobs.intKnob("SQUEEZEFS_READ_ADMISSION_FILL_PCT", u64, env));
        const epoch_ms = knobs.intKnob("SQUEEZEFS_TEST_READ_ADMISSION_EPOCH_MS", u64, env);
        if (epoch_ms != 0) g.epoch_ns = epoch_ms * std.time.ns_per_ms;
        return g;
    }

    /// Test seam: a shorter epoch so the roll is observable.
    pub fn setEpochNsForTest(self: *Governor, ns: u64) void {
        std.debug.assert(@import("builtin").is_test);
        self.epoch_ns = ns;
    }

    /// The CAS winner rolls: `cur → prev` (0 when the epochs are not
    /// adjacent), tokens minted ONCE from the previous epoch's foreground
    /// (leftovers die — the earlier two-window spend arithmetic re-admitted
    /// a herd at every boundary, exactly 2× the budget).
    pub fn roll(self: *Governor, now_ns: u64) void {
        const idx = now_ns / self.epoch_ns;
        const cur = self.epoch_idx.load(.monotonic);
        if (idx <= cur) return;
        if (self.epoch_idx.cmpxchgStrong(cur, idx, .acq_rel, .monotonic) != null) return;
        const adjacent = idx == cur + 1;
        const ev = self.evicted_cur.swap(0, .monotonic);
        const wa = self.wasted_cur.swap(0, .monotonic);
        const fg = self.foreground_cur.swap(0, .monotonic);
        self.evicted_prev.store(if (adjacent) ev else 0, .monotonic);
        self.wasted_prev.store(if (adjacent) wa else 0, .monotonic);
        self.foreground_prev.store(if (adjacent) fg else 0, .monotonic);
        self.tokens.store((if (adjacent) fg else 0) * self.fill_pct / 100, .monotonic);
    }

    /// The clamp's denominator: ranged device reads and `DemandStream`
    /// fills (the stream's own device spend funds the trickle).
    pub fn noteForeground(self: *Governor, bytes: u64, now_ns: u64) void {
        self.roll(now_ns);
        _ = self.foreground_cur.fetchAdd(bytes, .monotonic);
    }

    /// A PROTECTED hot-tier victim: `waste = len − served`; a victim that
    /// served nothing and was not stream-admitted is `evicted_unhit`.
    pub fn onEviction(self: *Governor, v: hot_tier.Victim, now_ns: u64) void {
        if (v.class != .protected) return;
        self.roll(now_ns);
        _ = self.evicted_cur.fetchAdd(v.len, .monotonic);
        if (v.served_bytes < v.len) {
            const waste = v.len - v.served_bytes;
            _ = self.wasted_cur.fetchAdd(waste, .monotonic);
            stats.read_admission_wasted_bytes.add(waste);
        }
        if (v.served_bytes == 0 and !v.stream_admitted) stats.read_admission_evicted_unhit.inc();
    }

    /// Eviction-side ratio ON PURPOSE (an admission burst must not dilute
    /// the ratio and unclamp itself): `wasted ≥ 2 × block ∧ 2 × wasted ≥
    /// evicted` over the previous epoch.
    pub fn clampEngaged(self: *Governor, block: u64, now_ns: u64) bool {
        self.roll(now_ns);
        const wasted = self.wasted_prev.load(.monotonic);
        const evicted = self.evicted_prev.load(.monotonic);
        const clamped = wasted >= CLAMP_FLOOR_BLOCKS * block and 2 * wasted >= evicted;
        stats.read_admission_governor_clamped.set(@intFromBool(clamped));
        return clamped;
    }

    /// ONE CAS spends a block's worth of tokens.
    pub fn reserveTokens(self: *Governor, block: u64) bool {
        var cur = self.tokens.load(.monotonic);
        while (true) {
            if (cur < block) return false;
            cur = self.tokens.cmpxchgWeak(cur, cur - block, .monotonic, .monotonic) orelse return true;
        }
    }

    pub fn allowEscalation(self: *Governor, block: u64, now_ns: u64) bool {
        if (!self.clampEngaged(block, now_ns) or self.reserveTokens(block)) return true;
        stats.read_admission_governor_denials.inc();
        return false;
    }

    pub fn allowStreamAdmission(self: *Governor, block: u64, now_ns: u64) bool {
        if (!self.clampEngaged(block, now_ns) or self.reserveTokens(block)) return true;
        stats.read_admission_stream_transients.inc();
        return false;
    }

    /// The non-reserving peek (the direct-drive prelude of rung 7).
    pub fn escalationWouldAdmit(self: *Governor, block: u64, now_ns: u64) bool {
        return !self.clampEngaged(block, now_ns) or self.tokens.load(.monotonic) >= block;
    }

    pub fn tokensLeft(self: *const Governor) u64 {
        return self.tokens.load(.monotonic);
    }
};

// ---------------------------------------------------------------------------
// The decision.
// ---------------------------------------------------------------------------

pub const Decision = struct {
    /// Where a > 256 KiB fill lands in RAM.
    hot_class: PutClass,
    /// The NVMe publish (admitted).
    publish: bool,
    ghost_hit: bool,
};

pub const Admission = struct {
    gpa: Allocator,
    policy: Policy,
    ghost: GhostTable,
    cooldown: EscalationCooldown,
    governor: Governor,

    pub fn init(gpa: Allocator, policy: Policy, fill_pct: u64) Allocator.Error!Admission {
        var ghost = try GhostTable.init(gpa);
        errdefer ghost.deinit(gpa);
        const cooldown = try EscalationCooldown.init(gpa);
        return .{ .gpa = gpa, .policy = policy, .ghost = ghost, .cooldown = cooldown, .governor = Governor.init(fill_pct) };
    }

    pub fn deinit(self: *Admission) void {
        self.ghost.deinit(self.gpa);
        self.cooldown.deinit(self.gpa);
        self.* = undefined;
    }

    /// The §1.5 table at the fill site. `len ≤ 256 KiB` always publishes
    /// (the read LRU's class); above it the policy decides. Counts
    /// `publishes_skipped` / `tier_admissions` (≥ 64 KiB) / `ghost_hits`.
    pub fn decideFill(self: *Admission, key: BlockKey, len: u64, class: FillClass, now_ns: u64) Decision {
        if (rsc.isSmallFill(len)) return .{ .hot_class = .protected, .publish = true, .ghost_hit = false };
        var hit = false;
        const admit = switch (self.policy) {
            .always => true,
            .never => false,
            .second_touch => blk: {
                hit = self.ghost.checkAndRecord(key);
                if (hit) stats.read_tier_admission_ghost_hits.inc();
                break :blk hit and (!class.streaming() or self.governor.allowStreamAdmission(len, now_ns));
            },
        };
        if (!admit) stats.read_fill_publishes_skipped.inc() else if (!rsc.isInlinePublish(len)) stats.read_tier_admissions.inc();
        const hot_class: PutClass = if (admit and self.policy == .second_touch)
            (if (class.streaming()) .protected_stream else .protected)
        else
            (if (class.streaming()) .probation_referenced else .probation);
        return .{ .hot_class = hot_class, .publish = admit, .ghost_hit = hit };
    }

    /// The ranged second touch (`ranged_escalation_candidate`): a ghost hit
    /// off Red, outside the key's cooldown and inside the governor's budget
    /// escalates to ONE whole-block fill — recorded in the cooldown, counted.
    pub fn rangedEscalation(self: *Admission, key: BlockKey, block: u64, red: bool, odirect: bool, now_ns: u64) bool {
        if (self.policy != .second_touch) return false;
        const hit = self.ghost.checkAndRecord(key);
        if (!hit) return false;
        stats.read_tier_admission_ghost_hits.inc();
        if (red or self.cooldown.recentlyEscalated(key, now_ns)) return false;
        if (!self.governor.allowEscalation(block, now_ns)) return false;
        self.cooldown.record(key, now_ns);
        stats.ranged_read_ghost_escalations.inc();
        if (odirect) stats.read_odirect_ghost_admits.inc();
        return true;
    }

    /// The hot tier's eviction sink: the governor's ledger for protected
    /// victims (the publish-of-protected-victims arm is the owner's).
    pub fn onHotVictim(self: *Admission, v: hot_tier.Victim) void {
        self.governor.onEviction(v, clock.monotonicNs());
    }
};

// ---------------------------------------------------------------------------

const testing = std.testing;

test "ghost: the second touch hits, the window slides one epoch at a time, a tag is never 0" {
    var g = try GhostTable.init(testing.allocator);
    defer g.deinit(testing.allocator);
    const k = BlockKey.init(1, 4 << 20);
    try testing.expect(!g.checkAndRecord(k));
    try testing.expect(g.checkAndRecord(k));
    // Roll one epoch: the previous-epoch tag still matches; two rolls do not.
    g.epoch.store(g.currentEpoch() + 1, .monotonic);
    try testing.expect(g.checkAndRecord(k));
    g.epoch.store(g.currentEpoch() + 2, .monotonic);
    try testing.expect(!g.checkAndRecord(k));
    try testing.expect(GhostTable.tag(0, 0) != 0);
    // 2^15 misses roll the epoch exactly once.
    const e0 = g.currentEpoch();
    var i: u64 = 0;
    while (i < GHOST_EPOCH_MISSES) : (i += 1) _ = g.checkAndRecord(.init(2, i << 22));
    try testing.expectEqual(e0 + 1, g.currentEpoch());
}

test "cooldown: a key re-escalates only after two 32 s epochs" {
    var c = try EscalationCooldown.init(testing.allocator);
    defer c.deinit(testing.allocator);
    const k = BlockKey.init(3, 0);
    const t0: u64 = 100 * std.time.ns_per_s;
    try testing.expect(!c.recentlyEscalated(k, t0));
    c.record(k, t0);
    try testing.expect(c.recentlyEscalated(k, t0));
    try testing.expect(c.recentlyEscalated(k, t0 + COOLDOWN_EPOCH_NS));
    try testing.expect(!c.recentlyEscalated(k, t0 + 2 * COOLDOWN_EPOCH_NS + 1));
}

test "governor: waste → clamp → denials by ONE token CAS; tokens minted once from the previous epoch's foreground; fitting sets never clamp" {
    var g = Governor.init(5);
    const block: u64 = 4 << 20;
    var now: u64 = 10 * GOVERNOR_EPOCH_NS;
    // Epoch A: 1 GiB of foreground, 4 protected victims that served nothing.
    g.noteForeground(derive_GiB, now);
    const unhit_before = stats.read_admission_evicted_unhit.load();
    var i: u32 = 0;
    while (i < 4) : (i += 1) g.onEviction(.{ .key = .init(1, i * block), .slot = 0, .len = @intCast(block), .class = .protected, .served_bytes = 0, .stream_admitted = false }, now);
    try testing.expectEqual(unhit_before + 4, stats.read_admission_evicted_unhit.load());
    // Probation victims and partial serves are not waste.
    g.onEviction(.{ .key = .init(1, 0), .slot = 0, .len = @intCast(block), .class = .probation, .served_bytes = 0, .stream_admitted = false }, now);
    g.onEviction(.{ .key = .init(1, 0), .slot = 0, .len = @intCast(block), .class = .protected, .served_bytes = block, .stream_admitted = false }, now);
    // Still epoch A: the clamp reads the PREVIOUS epoch (0) — not engaged.
    try testing.expect(!g.clampEngaged(block, now));
    try testing.expect(g.allowEscalation(block, now));
    // Epoch B: wasted_prev = 4 blocks ≥ 2 ∧ 2 × 4 ≥ 5 evicted ⇒ clamped;
    // tokens = 5 % of 1 GiB = 51.2 MiB = 12 whole blocks.
    now += GOVERNOR_EPOCH_NS;
    try testing.expect(g.clampEngaged(block, now));
    try testing.expectEqual(derive_GiB * 5 / 100, g.tokensLeft());
    const denials_before = stats.read_admission_governor_denials.load();
    var admits: u32 = 0;
    i = 0;
    while (i < 20) : (i += 1) admits += @intFromBool(g.allowEscalation(block, now));
    try testing.expectEqual(@as(u32, 12), admits);
    try testing.expectEqual(denials_before + 8, stats.read_admission_governor_denials.load());
    try testing.expect(!g.escalationWouldAdmit(block, now));
    const trans_before = stats.read_admission_stream_transients.load();
    try testing.expect(!g.allowStreamAdmission(block, now));
    try testing.expectEqual(trans_before + 1, stats.read_admission_stream_transients.load());
    // Epoch C with nothing in B: prev = 0 ⇒ unclamped; a non-adjacent jump
    // zeroes prev too.
    now += GOVERNOR_EPOCH_NS;
    try testing.expect(!g.clampEngaged(block, now));
    g.noteForeground(derive_GiB, now);
    now += 5 * GOVERNOR_EPOCH_NS;
    g.roll(now);
    try testing.expectEqual(@as(u64, 0), g.tokensLeft());
}

const derive_GiB: u64 = 1 << 30;

test "the §1.5 decision table" {
    var a = try Admission.init(testing.allocator, .second_touch, 5);
    defer a.deinit();
    const big: u64 = 4 << 20;
    const k = BlockKey.init(1, 0);
    const now: u64 = 10 * GOVERNOR_EPOCH_NS;
    // ≤ 256 KiB: always publish (the LRU class).
    const small = a.decideFill(k, rsc.SMALL_FILL_MAX, .demand, now);
    try testing.expect(small.publish and !small.ghost_hit);
    // First touch, demand: probation, publish skipped.
    const skipped_before = stats.read_fill_publishes_skipped.load();
    const d1 = a.decideFill(k, big, .demand, now);
    try testing.expectEqual(PutClass.probation, d1.hot_class);
    try testing.expect(!d1.publish and !d1.ghost_hit);
    try testing.expectEqual(skipped_before + 1, stats.read_fill_publishes_skipped.load());
    // Second touch, demand: protected + publish.
    const adm_before = stats.read_tier_admissions.load();
    const d2 = a.decideFill(k, big, .demand, now);
    try testing.expectEqual(PutClass.protected, d2.hot_class);
    try testing.expect(d2.publish and d2.ghost_hit);
    try testing.expectEqual(adm_before + 1, stats.read_tier_admissions.load());
    // First touch, streaming: probation_referenced (the one-lap grace), skipped.
    const k2 = BlockKey.init(1, big);
    const d3 = a.decideFill(k2, big, .demand_stream, now);
    try testing.expectEqual(PutClass.probation_referenced, d3.hot_class);
    try testing.expect(!d3.publish);
    // Second touch, streaming, governor unclamped: protected_stream + publish.
    const d4 = a.decideFill(k2, big, .prefetch, now);
    try testing.expectEqual(PutClass.protected_stream, d4.hot_class);
    try testing.expect(d4.publish);
    // `always`: publish on first touch, RAM class probation.
    var always = try Admission.init(testing.allocator, .always, 5);
    defer always.deinit();
    const d5 = always.decideFill(k, big, .demand, now);
    try testing.expectEqual(PutClass.probation, d5.hot_class);
    try testing.expect(d5.publish);
    var never = try Admission.init(testing.allocator, .never, 5);
    defer never.deinit();
    const d6 = never.decideFill(k, big, .demand_stream, now);
    try testing.expectEqual(PutClass.probation_referenced, d6.hot_class);
    try testing.expect(!d6.publish);
    // Ranged escalation: first touch no; second touch yes once; then the cooldown.
    const k3 = BlockKey.init(1, 2 * big);
    try testing.expect(!a.rangedEscalation(k3, big, false, false, now));
    const esc_before = stats.ranged_read_ghost_escalations.load();
    const od_before = stats.read_odirect_ghost_admits.load();
    try testing.expect(a.rangedEscalation(k3, big, false, true, now));
    try testing.expectEqual(esc_before + 1, stats.ranged_read_ghost_escalations.load());
    try testing.expectEqual(od_before + 1, stats.read_odirect_ghost_admits.load());
    try testing.expect(!a.rangedEscalation(k3, big, false, false, now));
    // Red pauses escalation; `always`/`never` never escalate.
    const k4 = BlockKey.init(1, 3 * big);
    _ = a.rangedEscalation(k4, big, false, false, now);
    try testing.expect(!a.rangedEscalation(k4, big, true, false, now));
    try testing.expect(!always.rangedEscalation(k4, big, false, false, now));
}

test "policy: knob parsing and the hot-budget-0 degrade" {
    const gpa = testing.allocator;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    try testing.expectEqual(Policy.second_touch, Policy.fromEnv(&env, 1));
    try testing.expectEqual(Policy.always, Policy.fromEnv(&env, 0));
    try env.put("SQUEEZEFS_READ_TIER_ADMISSION", "never");
    try testing.expectEqual(Policy.never, Policy.fromEnv(&env, 0));
    try env.put("SQUEEZEFS_READ_TIER_ADMISSION", "ALWAYS");
    try testing.expectEqual(Policy.always, Policy.fromEnv(&env, 1));
    try testing.expectEqual(@as(u64, 5), Governor.fromEnv(&env).fill_pct);
    try testing.expectEqual(GOVERNOR_EPOCH_NS, Governor.fromEnv(&env).epoch_ns);
    try env.put("SQUEEZEFS_TEST_READ_ADMISSION_EPOCH_MS", "50");
    try testing.expectEqual(@as(u64, 50 * std.time.ns_per_ms), Governor.fromEnv(&env).epoch_ns);
}

fn policyNameFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [64]u8 = undefined;
    const len = smith.slice(&buf);
    _ = Policy.parse(buf[0..len]);
}

test "fuzz: the admission policy name parser is total on arbitrary text" {
    try fuzzkit.fuzz({}, policyNameFuzz, .{});
}
