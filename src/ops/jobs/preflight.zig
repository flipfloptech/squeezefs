//! The capacity preflight (charter §8 "capacity preflight `avail ≥ needed +
//! transient + headroom` re-verified at every checkpoint"; spec §2.5.1):
//! every term DERIVED, the closed form in `set_config.CapacityPreflight`
//! (the record module owns the constants so the format and the verbs
//! agree on one number):
//!
//!   needed    = distinct_used_blocks(V) × block     the census, DEDUPED per block
//!   avail     = Σ_{b ≠ V, eligible, cap > 0} (cap − used)   cap 0 contributes 0
//!   transient = 64 × ACTUAL_workers × block          the in-flight double-count window
//!   headroom  = max(write_rate_est × drain_eta, 1 GiB)
//!
//! `write_rate_est` is the trailing-≤ 60 s mean of foreground byte
//! admission sampled at checkpoint cadence (0 until two samples);
//! `drain_eta` starts at the measured 512 MiB/s floor and becomes `needed
//! / measured_job_rate` from the first checkpoint. Re-verified at EVERY
//! checkpoint: a failure parks the drain `PausedCapacity`, loud, with the
//! refusal text. Portable.

const std = @import("std");
const set_config = @import("../../data/set_config.zig");

pub const CapacityPreflight = set_config.CapacityPreflight;
pub const transientBytes = set_config.transientBytes;
pub const headroomBytes = set_config.headroomBytes;
pub const initialDrainEtaS = set_config.initialDrainEtaS;
pub const HEADROOM_FLOOR: u64 = set_config.DRAIN_HEADROOM_FLOOR_BYTES;
pub const INFLIGHT_WINDOW_BLOCKS: u64 = set_config.EVACUATE_INFLIGHT_WINDOW_BLOCKS;
pub const FLOOR_RATE_BPS: u64 = set_config.DRAIN_FLOOR_RATE_BYTES_PER_SEC;

/// One placement row the preflight sums over.
pub const SurvivorRow = struct {
    vol_tag: u64,
    eligible: bool,
    capacity_bytes: u64,
    used_bytes: u64,
};

/// Σ free over the placement-eligible survivors (`victim` excluded); an
/// unbounded row (cap 0) contributes 0 — conservative.
pub fn availBytes(rows: []const SurvivorRow, victim: u64) u64 {
    var sum: u64 = 0;
    for (rows) |r| {
        if (r.vol_tag == victim or !r.eligible or r.capacity_bytes == 0) continue;
        sum +|= r.capacity_bytes -| r.used_bytes;
    }
    return sum;
}

/// The drain's eta: the floor rate before the first checkpoint, the
/// measured job rate after (`moved_bytes / elapsed_s`, 0 ⇒ the floor).
pub fn drainEtaS(needed: u64, measured_rate_bps: u64) u64 {
    if (measured_rate_bps == 0) return initialDrainEtaS(needed);
    return needed / measured_rate_bps + 1;
}

pub const Inputs = struct {
    needed: u64,
    rows: []const SurvivorRow,
    victim: u64,
    workers: u64,
    block_size: u64,
    write_rate_bps: u64,
    measured_job_rate_bps: u64 = 0,
};

/// Every term from the inputs.
pub fn compute(in: Inputs) CapacityPreflight {
    return .{
        .needed = in.needed,
        .avail = availBytes(in.rows, in.victim),
        .transient = transientBytes(in.workers, in.block_size),
        .headroom = headroomBytes(in.write_rate_bps, drainEtaS(in.needed, in.measured_job_rate_bps)),
    };
}

/// The trailing-≤ 60 s mean of foreground byte admission, sampled at
/// checkpoint cadence; 0 until two samples. The window bounds a burst's
/// influence to the span a drain's headroom covers.
pub const WriteRateEstimator = struct {
    pub const WINDOW_NS: u64 = 60 * std.time.ns_per_s;
    pub const SAMPLES: usize = 16;

    const Sample = struct { at_ns: u64, bytes: u64 };

    ring: [SAMPLES]Sample = undefined,
    head: usize = 0,
    len: usize = 0,

    /// `bytes` is the MONOTONE admitted-byte counter (`admit_write_*_bytes`).
    pub fn sample(self: *WriteRateEstimator, at_ns: u64, bytes: u64) void {
        self.ring[self.head] = .{ .at_ns = at_ns, .bytes = bytes };
        self.head = (self.head + 1) % SAMPLES;
        if (self.len < SAMPLES) self.len += 1;
    }

    /// Bytes/s between the oldest sample inside the window and the newest;
    /// 0 with fewer than two samples or a zero span.
    pub fn rateBps(self: *const WriteRateEstimator) u64 {
        if (self.len < 2) return 0;
        const newest = self.ring[(self.head + SAMPLES - 1) % SAMPLES];
        var oldest = newest;
        var i: usize = 1;
        while (i < self.len) : (i += 1) {
            const s = self.ring[(self.head + SAMPLES - 1 - i) % SAMPLES];
            if (newest.at_ns - s.at_ns > WINDOW_NS) break;
            oldest = s;
        }
        const span = newest.at_ns - oldest.at_ns;
        if (span == 0 or newest.bytes < oldest.bytes) return 0;
        return @intCast(@as(u128, newest.bytes - oldest.bytes) * std.time.ns_per_s / span);
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "every term is derived: transient = 64 × workers × block; headroom = max(rate × eta, 1 GiB); eta = needed / 512 MiB/s + 1 before the first checkpoint, needed / measured + 1 after; avail excludes the victim, the ineligible and the unbounded" {
    const block: u64 = 4 << 20;
    try testing.expectEqual(@as(u64, 64 * 3 * block), transientBytes(3, block));
    try testing.expectEqual(HEADROOM_FLOOR, headroomBytes(0, 1000));
    try testing.expectEqual(@as(u64, 2 << 30), headroomBytes(1 << 30, 2));
    try testing.expectEqual(@as(u64, 1), initialDrainEtaS(0));
    try testing.expectEqual(@as(u64, 17), drainEtaS(8 << 30, 0)); // 8 GiB / 512 MiB/s = 16 (+1)
    try testing.expectEqual(@as(u64, 9), drainEtaS(8 << 30, 1 << 30));
    const rows = [_]SurvivorRow{
        .{ .vol_tag = 1, .eligible = true, .capacity_bytes = 100, .used_bytes = 40 }, // victim
        .{ .vol_tag = 2, .eligible = true, .capacity_bytes = 100, .used_bytes = 30 },
        .{ .vol_tag = 3, .eligible = false, .capacity_bytes = 100, .used_bytes = 0 },
        .{ .vol_tag = 4, .eligible = true, .capacity_bytes = 0, .used_bytes = 0 },
        .{ .vol_tag = 5, .eligible = true, .capacity_bytes = 50, .used_bytes = 60 }, // over-full: 0
    };
    try testing.expectEqual(@as(u64, 70), availBytes(&rows, 1));
}

test "preflight property: refuse iff avail < needed + transient + headroom, over fuzzed censuses" {
    var prng = std.Random.DefaultPrng.init(0x70726566);
    const rnd = prng.random();
    const block: u64 = 4 << 20;
    var round: usize = 0;
    var refused: usize = 0;
    while (round < 2000) : (round += 1) {
        var rows: [6]SurvivorRow = undefined;
        for (&rows, 0..) |*r, i| {
            const cap = rnd.uintAtMost(u64, 1 << 40);
            r.* = .{ .vol_tag = i + 1, .eligible = rnd.boolean(), .capacity_bytes = cap, .used_bytes = rnd.uintAtMost(u64, cap) };
        }
        const in: Inputs = .{ .needed = rnd.uintAtMost(u64, 1 << 40), .rows = &rows, .victim = 1, .workers = 1 + rnd.uintLessThan(u64, 8), .block_size = block, .write_rate_bps = rnd.uintAtMost(u64, 1 << 30), .measured_job_rate_bps = if (rnd.boolean()) 0 else rnd.uintAtMost(u64, 2 << 30) };
        const p = compute(in);
        const expect_refuse = p.avail < p.needed +| p.transient +| p.headroom;
        try testing.expectEqual(expect_refuse, p.refuses());
        try testing.expectEqual(availBytes(&rows, 1), p.avail);
        try testing.expectEqual(transientBytes(in.workers, block), p.transient);
        try testing.expect(p.headroom >= HEADROOM_FLOOR);
        if (p.refuses()) refused += 1;
    }
    try testing.expect(refused > 0 and refused < 2000);
    // The refusal prints every term.
    var buf: [256]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try (CapacityPreflight{ .avail = 1, .needed = 2, .transient = 3, .headroom = 4 }).write(&w);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "REFUSES") != null);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "transient 3 B") != null);
}

test "WriteRateEstimator: 0 until two samples; the trailing mean over ≤ 60 s; a counter reset reads 0; samples older than the window drop out" {
    var e: WriteRateEstimator = .{};
    try testing.expectEqual(@as(u64, 0), e.rateBps());
    e.sample(0, 0);
    try testing.expectEqual(@as(u64, 0), e.rateBps());
    e.sample(10 * std.time.ns_per_s, 100 << 20);
    try testing.expectEqual(@as(u64, 10 << 20), e.rateBps());
    e.sample(20 * std.time.ns_per_s, 400 << 20);
    try testing.expectEqual(@as(u64, 20 << 20), e.rateBps());
    // Samples older than 60 s before the newest drop out: from 120 s the
    // window holds 80 s and 120 s only.
    e.sample(80 * std.time.ns_per_s, 450 << 20);
    e.sample(120 * std.time.ns_per_s, 490 << 20);
    try testing.expectEqual(@as(u64, 1 << 20), e.rateBps());
    e.sample(130 * std.time.ns_per_s, 0);
    try testing.expectEqual(@as(u64, 0), e.rateBps());
}
