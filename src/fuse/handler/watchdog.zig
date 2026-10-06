//! The deadline watchdog (`docs/porting/fuse_handlers_lanes.md` §6; charter
//! §4 "a deadline watchdog that logs overdue ops loudly (per-op timeout
//! wrappers are BANNED — dropping a handler mid-commit is the ring-wedge
//! vector)"; decision 0003 Q5).
//!
//! What it watches: the op REGISTRY — every handler claims a slot at entry
//! (`Registry.claim`) and releases it at its reply; the slot carries the
//! opcode, the ino, the unique, a COARSE start stamp and the STATION (the
//! last `op_trace.Stage` the handler stamped), so an overdue report names
//! the await the op is parked at.
//!
//! What it does: every `TICK_NS` (5 s, D1.b "one per-daemon task ticks
//! every 5 s") it refreshes the coarse clock, derives each op class's
//! threshold and logs every op past it LOUDLY — op, ino, unique, age,
//! station — counting `fuse_op_watchdog_overdue` per OBSERVATION (one per
//! scan per stuck op, so a still-stuck op keeps the signal alive) and, when
//! anything is overdue, ONE wedge-census line built from process-global
//! atomics alone (readable no matter what is wedged: the zcws-9 W4 capture
//! had 16 k overdue lines and a `.stats` read that hung).
//!
//! What it NEVER does: cancel. The D1.b audit of every await a handler can
//! park on and its disposition — (1) fsync/fsyncdir barrier waits → bounded
//! INSIDE the coalescer (the one drop-safe layer); (2) ring-admission
//! parking → this watchdog + the journal's own escalation (a park past
//! `conveyor.PARK_ESCALATION_NS` trips the journal-failure latch; three
//! crossings latch FAILED → EIO; another committer's success resets); (3)
//! `completed_upto` waits → watchdog only; (4) demand-paged node reads →
//! watchdog only; (5) stripe/DLM locks → watchdog only ("deadlock-freedom
//! is the lock-order contract's job"); (6) data-path device I/O → watchdog
//! only (the old `max(TIMEOUT, 30 s)` read/write timeouts synthesized
//! ETIMEDOUT while leaving the device wedged; decision 0002 row 34). A
//! `timeout()` wrapper's expiry DROPS the handler at whatever await it is
//! parked on; reachable through every mutation handler is the commit, whose
//! accounting is unforgiving — an admission dropped between ring admission
//! and its reservation leaks ring budget forever, a registered reservation
//! whose future dies before completion stalls `completed_upto` for every
//! later committer. "The 30 s per-op timeout was therefore a live
//! volume-wedge vector on any commit that crossed the threshold."
//!
//! The threshold is DERIVED per op class (decision 0002 ledger: the
//! reference's `SQUEEZEFS_TIMEOUT` was a free-floating 30 s with a knob on
//! top): `max(TAIL_FACTOR × p99 of the class's always-on latency histogram,
//! THRESHOLD_FLOOR_NS)`. WHY 4 × p99: an op four times slower than 99 % of
//! its own class in the same window is waiting on something, not working —
//! a slow substrate raises every class's tail together and the threshold
//! follows it instead of flooding the log, while a wedge (whose victims
//! never complete and so never move the histogram) is named within one
//! tick of crossing. WHY the 5 s floor: the start stamp is the coarse
//! clock, refreshed once per tick, so an age is overstated by up to one
//! tick — below one tick the error would exceed the threshold itself.

const std = @import("std");
const Io = std.Io;
const assert = std.debug.assert;
const abi = @import("../abi.zig");
const codec = @import("../codec.zig");
const geometry = @import("../geometry.zig");
const op_trace = @import("op_trace.zig");
const counters = @import("../../core/counters.zig");
const clock = @import("../../core/clock.zig");
const derive = @import("../../core/derive.zig");
const log = @import("../../core/log.zig");
const tick_mod = @import("../../sync/tick.zig");
const timer_heap = @import("../../exec/timer_heap.zig");

/// The tick: D1.b's cadence, and the coarse clock's refresh period (age
/// error ≤ one tick).
pub const TICK_NS: u64 = 5 * std.time.ns_per_s;
/// The threshold floor = one tick (module doc).
pub const THRESHOLD_FLOOR_NS: u64 = TICK_NS;
/// The tail factor over the class's p99 (module doc).
pub const TAIL_FACTOR: u64 = 4;
/// Slots per shard: 2 × the transport's desired queue depth — headroom for
/// sideband and in-process registrations beside a full ring queue
/// (fuse_client.rs:1418–1422).
pub const SLOTS_PER_SHARD: u32 = 2 * geometry.Q_DEPTH_DESIRED;
/// The shipped slab: the never-regress floor (the field rig needed 1024 —
/// 32 CPUs × depth 32 — and the 256-slot single-cursor slab spent 6.65 % of
/// fused-worker cycles on failing CASes at 525 k IOPS).
pub const SHIPPED_SLOTS: u32 = 256;

/// `shards = max(possible_cpus, ceil(SHIPPED / SLOTS_PER_SHARD))`: one
/// shard per queue (the queue population IS the delivering population),
/// never below the shipped slab.
pub fn derivedShards(possible_cpus: u32) u32 {
    return @max(@max(possible_cpus, 1), @as(u32, @intCast(derive.ceilDiv(SHIPPED_SLOTS, SLOTS_PER_SHARD))));
}

/// `max(TAIL_FACTOR × p99, floor)`.
pub fn thresholdNs(p99_upper_ns: u64) u64 {
    return @max(p99_upper_ns *| TAIL_FACTOR, THRESHOLD_FLOOR_NS);
}

/// The upper bound of histogram bucket `i` in ns: bucket 0 is `≤ 1 µs`,
/// bucket i covers `(2^(i−1), 2^i] µs`; the open top bucket (`> 16 s`)
/// reads as the next power (2^25 µs ≈ 33.5 s) so a tail there still
/// derives a finite threshold.
pub fn bucketUpperNs(i: usize) u64 {
    assert(i < counters.hist_buckets);
    return (@as(u64, 1) << @intCast(i)) * std.time.ns_per_us;
}

/// The smallest bucket upper bound at or above the p-th percentile of a
/// snapshot; 0 for an empty snapshot (the floor then rules).
pub fn percentileUpperNs(s: counters.LatencyHistogram.Snapshot, percent: u64) u64 {
    if (s.count == 0) return 0;
    const need = derive.ceilDiv(s.count * percent, 100);
    var cum: u64 = 0;
    for (s.buckets, 0..) |n, i| {
        cum += n;
        if (cum >= need) return bucketUpperNs(i);
    }
    return bucketUpperNs(counters.hist_buckets - 1);
}

pub const Thresholds = [op_trace.OpClass.count]u64;

/// Every class's threshold from its always-on latency histogram.
pub fn deriveThresholds() Thresholds {
    var t: Thresholds = undefined;
    for (&t, 0..) |*v, i| v.* = thresholdNs(percentileUpperNs(op_trace.latencyHistogram(@fromBackingInt(@intCast(i))).snapshot(), 99));
    return t;
}

/// `fuse_op_watchdog_overdue`: overdue OBSERVATIONS (0 healthy).
pub var fuse_op_watchdog_overdue: counters.ShardedCounter = .{};
/// `fuse_ops_inflight`: claimed registry slots at the last tick (the wedge
/// census's own gauge).
pub var fuse_ops_inflight: counters.Gauge = .{};
/// `fuse_op_registry_exhausted`: claims that found every slot taken — the
/// op ran unregistered (watchdog-blind). Growth means the geometry no
/// longer shadows the delivered concurrency.
pub var fuse_op_registry_exhausted: counters.ShardedCounter = .{};
/// `fuse_op_registry_slots`: the registry's capacity (geometry gauge).
pub var fuse_op_registry_slots: counters.Gauge = .{};

/// One in-flight op. 64-byte aligned: ~2.7 packed slots per line made a
/// FAILING `lock cmpxchg` take the line exclusive from a neighbour's claim.
pub const OpSlot = struct {
    /// 0 free, 1 claimed. The claim is an acq_rel CAS (paired with the
    /// release store in `release`) so a reader of a claimed slot's fields
    /// sees the claimer's writes.
    state: std.atomic.Value(u32) align(64) = .init(0),
    /// The last stage stamped (`op_trace.Stage`'s id; 0 = none yet).
    station: std.atomic.Value(u8) = .init(0),
    opcode: u32 = 0,
    ino: u64 = 0,
    unique: u64 = 0,
    /// The coarse clock at the claim.
    start_ns: u64 = 0,

    pub fn setStation(self: *OpSlot, s: op_trace.Stage) void {
        self.station.store(@backingInt(s), .monotonic);
    }
};

comptime {
    assert(@sizeOf(OpSlot) == 64);
    assert(@alignOf(OpSlot) == 64);
}

/// The claimer's home shard: assigned round-robin on a thread's first
/// claim (futures do not migrate; a thread keeps its shard).
threadlocal var home_shard: u32 = std.math.maxInt(u32);
var next_home: std.atomic.Value(u32) = .init(0);

pub const Registry = struct {
    gpa: std.mem.Allocator,
    shards: u32,
    slots: []OpSlot,
    /// Refreshed once per tick; every claim stamps it (zero clock syscalls
    /// on the per-op path — the whole point of retiring `timeout()`'s
    /// timer-wheel + vdso tax, 2.75 % of daemon cycles).
    coarse_now_ns: std.atomic.Value(u64),

    pub fn init(gpa: std.mem.Allocator, shards: u32) std.mem.Allocator.Error!Registry {
        assert(shards > 0);
        const slots = try gpa.alloc(OpSlot, @as(usize, shards) * SLOTS_PER_SHARD);
        @memset(slots, .{});
        fuse_op_registry_slots.set(slots.len);
        return .{ .gpa = gpa, .shards = shards, .slots = slots, .coarse_now_ns = .init(clock.monotonicNs()) };
    }

    pub fn deinit(self: *Registry) void {
        self.gpa.free(self.slots);
        self.* = undefined;
    }

    pub fn capacity(self: *const Registry) usize {
        return self.slots.len;
    }

    fn myShard(self: *const Registry) u32 {
        if (home_shard == std.math.maxInt(u32)) home_shard = next_home.fetchAdd(1, .monotonic);
        return home_shard % self.shards;
    }

    /// Claim a slot: the home shard first, then every other shard; a
    /// relaxed load screens a taken slot before the CAS so a scan never
    /// bounces lines it cannot take. Exhaustion ⇒ null (the op runs
    /// unregistered, counted).
    pub fn claim(self: *Registry, opcode: abi.Opcode, ino: u64, unique: u64) ?*OpSlot {
        const home = self.myShard();
        var attempt: u32 = 0;
        while (attempt < self.shards) : (attempt += 1) {
            const shard = (home + attempt) % self.shards;
            const base = @as(usize, shard) * SLOTS_PER_SHARD;
            for (self.slots[base .. base + SLOTS_PER_SHARD]) |*s| {
                if (s.state.load(.monotonic) != 0) continue;
                if (s.state.cmpxchgWeak(0, 1, .acq_rel, .monotonic) != null) continue;
                s.opcode = @backingInt(opcode);
                s.ino = ino;
                s.unique = unique;
                s.start_ns = self.coarse_now_ns.load(.monotonic);
                s.station.store(0, .monotonic);
                return s;
            }
        }
        fuse_op_registry_exhausted.inc();
        return null;
    }

    /// Any thread may release (release pairs with the claim's acquire).
    pub fn release(self: *Registry, s: *OpSlot) void {
        _ = self;
        assert(s.state.load(.monotonic) == 1);
        s.state.store(0, .release);
    }

    pub fn refreshCoarse(self: *Registry, now_ns: u64) void {
        self.coarse_now_ns.store(now_ns, .monotonic);
    }

    pub fn inFlight(self: *const Registry) u32 {
        var n: u32 = 0;
        for (self.slots) |*s| {
            if (s.state.load(.acquire) != 0) n += 1;
        }
        return n;
    }

    pub const TickReport = struct { in_flight: u32, overdue: u32 };

    /// One scan at `now_ns` against `thresholds` (per class). Logs every
    /// overdue op and, when any is, the wedge census. Never touches a reply.
    pub fn tick(self: *Registry, now_ns: u64, thresholds: *const Thresholds) TickReport {
        var report: TickReport = .{ .in_flight = 0, .overdue = 0 };
        for (self.slots) |*s| {
            // acquire pairs with the claim's acq_rel CAS: the fields below
            // are the claimer's.
            if (s.state.load(.acquire) == 0) continue;
            report.in_flight += 1;
            const op = abi.Opcode.fromInt(s.opcode);
            const age = now_ns -| s.start_ns;
            const thr = thresholds[@backingInt(op_trace.OpClass.of(op))];
            if (age < thr) continue;
            report.overdue += 1;
            fuse_op_watchdog_overdue.inc();
            log.err("FUSE op watchdog: {s} (ino {d}, unique {d}) in flight for {d} ms (> {d} ms), parked past station [{s}] — op is NOT cancelled (D1.b: no ETIMEDOUT synthesis; a dropped committer is the ring-wedge vector)", .{ op.name(), s.ino, s.unique, age / std.time.ns_per_ms, thr / std.time.ns_per_ms, op_trace.Stage.nameOf(s.station.load(.monotonic)) });
        }
        fuse_ops_inflight.set(report.in_flight);
        if (report.overdue > 0) logWedgeCensus(report);
        return report;
    }
};

/// The one-line wedge census: process-global atomics only, so it is
/// readable from the watchdog no matter what is wedged.
fn logWedgeCensus(report: Registry.TickReport) void {
    log.err("wedge census: fuse_ops_inflight={d} overdue_now={d} fuse_op_watchdog_overdue={d} fuse_requests_malformed={d} lock_ticked_reregisters={d} channel_ticked_reregisters={d} invariant_tripwires={d} registry_exhausted={d}", .{
        report.in_flight,
        report.overdue,
        fuse_op_watchdog_overdue.load(),
        codec.requests_malformed.load(),
        tick_mod.lock_ticked_reregisters.load(),
        tick_mod.channel_ticked_reregisters.load(),
        counters.invariantTripwires(),
        fuse_op_registry_exhausted.load(),
    });
}

/// The watchdog task: ONE `TICK_NS` interval on the caller's `Io` (the
/// daemon runs it off a lane — a raw thread's `sleep` rides `sqz-timer` —
/// because it must survive a broken scheduler: it IS the wedge instrument).
pub const Watchdog = struct {
    registry: *Registry,
    stop: std.atomic.Value(bool) = .init(false),
    ticks: std.atomic.Value(u64) = .init(0),

    pub fn init(registry: *Registry) Watchdog {
        return .{ .registry = registry };
    }

    /// One tick at `now_ns`: refresh the coarse clock, derive the
    /// thresholds, scan.
    pub fn tickOnce(self: *Watchdog, now_ns: u64) Registry.TickReport {
        self.registry.refreshCoarse(now_ns);
        const thresholds = deriveThresholds();
        const r = self.registry.tick(now_ns, &thresholds);
        _ = self.ticks.fetchAdd(1, .monotonic);
        return r;
    }

    /// The loop: `Interval` keeps the cadence drift-free; `requestStop`
    /// ends it at the next tick.
    pub fn run(self: *Watchdog, io: Io) void {
        var interval = timer_heap.Interval.init(clock.monotonicNs(), TICK_NS);
        while (!self.stop.load(.acquire)) {
            const now = clock.monotonicNs();
            if (now < interval.deadline()) {
                io.sleep(.fromNanoseconds(@intCast(interval.deadline() - now)), .awake) catch return;
            }
            const landed = clock.monotonicNs();
            _ = self.tickOnce(landed);
            interval.advance(landed);
        }
    }

    pub fn requestStop(self: *Watchdog) void {
        self.stop.store(true, .release);
    }
};

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

const testing = std.testing;

test "geometry ties: shards = max(cpus, 4), 64 slots per shard, total ≥ the shipped 256 and ≥ 2× the delivered ring capacity" {
    try testing.expectEqual(@as(u32, 4), derivedShards(1));
    try testing.expectEqual(@as(u32, 4), derivedShards(4));
    try testing.expectEqual(@as(u32, 32), derivedShards(32));
    try testing.expectEqual(@as(u32, 64), SLOTS_PER_SHARD);
    try testing.expect(derivedShards(1) * SLOTS_PER_SHARD >= SHIPPED_SLOTS);
    // 32 CPUs × depth 32 = 1024 delivered; the registry holds 2048.
    try testing.expectEqual(@as(u32, 2048), derivedShards(32) * SLOTS_PER_SHARD);
    try testing.expect(derivedShards(32) * SLOTS_PER_SHARD >= 2 * 32 * geometry.Q_DEPTH_DESIRED);
}

test "threshold derivation: max(4 × p99, 5 s); an empty class derives the floor; the open top bucket still derives a finite number" {
    try testing.expectEqual(THRESHOLD_FLOOR_NS, thresholdNs(0));
    try testing.expectEqual(THRESHOLD_FLOOR_NS, thresholdNs(std.time.ns_per_s));
    try testing.expectEqual(8 * std.time.ns_per_s, thresholdNs(2 * std.time.ns_per_s));
    var s: counters.LatencyHistogram.Snapshot = .{};
    try testing.expectEqual(@as(u64, 0), percentileUpperNs(s, 99));
    // 99 of 100 samples at ≤ 1 µs: the 99th percentile IS the first bucket.
    s.buckets[0] = 99;
    s.buckets[counters.bucketIndex(3000)] = 1;
    s.count = 100;
    try testing.expectEqual(std.time.ns_per_us, percentileUpperNs(s, 99));
    // 98 + 2: p99 lands on the 3 ms sample's bucket (its upper bound, 4096 µs).
    s.buckets[0] = 98;
    s.buckets[counters.bucketIndex(3000)] = 2;
    const ms3_upper = bucketUpperNs(counters.bucketIndex(3000));
    try testing.expectEqual(ms3_upper, percentileUpperNs(s, 99));
    try testing.expect(ms3_upper > 3 * std.time.ns_per_ms and ms3_upper < 5 * std.time.ns_per_ms);
    try testing.expectEqual(THRESHOLD_FLOOR_NS, thresholdNs(percentileUpperNs(s, 99)));
    // A class whose p99 is in the open top bucket derives 4 × its finite bound, not infinity.
    var top: counters.LatencyHistogram.Snapshot = .{};
    top.buckets[counters.hist_buckets - 1] = 10;
    top.count = 10;
    const top_upper = bucketUpperNs(counters.hist_buckets - 1);
    try testing.expect(top_upper > 16 * std.time.ns_per_s);
    try testing.expectEqual(top_upper, percentileUpperNs(top, 99));
    try testing.expectEqual(4 * top_upper, thresholdNs(percentileUpperNs(top, 99)));
    try testing.expectEqual(std.time.ns_per_us, bucketUpperNs(0));
    try testing.expectEqual(2 * std.time.ns_per_us, bucketUpperNs(1));
}

test "registry: claim/release recycle; a slot owns its cache line; exhaustion degrades to unregistered and counts; release from a foreign thread recycles" {
    var reg = try Registry.init(testing.allocator, 1);
    defer reg.deinit();
    try testing.expectEqual(@as(usize, 64), reg.capacity());
    var held: [64]*OpSlot = undefined;
    for (&held, 0..) |*h, i| h.* = reg.claim(.lookup, @intCast(i), @intCast(2 * i)).?;
    try testing.expectEqual(@as(u32, 64), reg.inFlight());
    const exhausted_before = fuse_op_registry_exhausted.load();
    try testing.expectEqual(@as(?*OpSlot, null), reg.claim(.read, 99, 999));
    try testing.expectEqual(exhausted_before + 1, fuse_op_registry_exhausted.load());
    // A foreign thread releases one; the next claim recycles it.
    const Releaser = struct {
        fn run(r: *Registry, s: *OpSlot) void {
            r.release(s);
        }
    };
    const t = try std.Thread.spawn(.{}, Releaser.run, .{ &reg, held[17] });
    t.join();
    try testing.expectEqual(@as(u32, 63), reg.inFlight());
    const again = reg.claim(.write, 7, 77).?;
    try testing.expectEqual(held[17], again);
    try testing.expectEqual(@as(u64, 7), again.ino);
    for (held) |h| reg.release(h);
    try testing.expectEqual(@as(u32, 0), reg.inFlight());
    // Two adjacent slots are 64 bytes apart: no shared line.
    try testing.expectEqual(@as(usize, 64), @intFromPtr(&reg.slots[1]) - @intFromPtr(&reg.slots[0]));
}

test "watchdog tick: an op past its class threshold is reported with op/ino/age/station and counted per observation; it vanishes at release; the scan covers every shard" {
    var reg = try Registry.init(testing.allocator, 3);
    defer reg.deinit();
    var wd = Watchdog.init(&reg);
    const t0: u64 = 1_000 * std.time.ns_per_s;
    reg.refreshCoarse(t0);
    // One op per shard (the home shard rotates per thread, so force the
    // spread by filling: claims land in the caller's home shard first; the
    // scan must still see slots in every shard).
    const a = reg.claim(.write, 11, 101).?;
    a.setStation(.meta_backend_start);
    const far = &reg.slots[2 * SLOTS_PER_SHARD + 5];
    try testing.expect(far.state.cmpxchgStrong(0, 1, .acq_rel, .monotonic) == null);
    far.opcode = @backingInt(abi.Opcode.lookup);
    far.ino = 12;
    far.unique = 103;
    far.start_ns = t0;
    // Fresh: nothing overdue.
    var r = wd.tickOnce(t0 + 1 * std.time.ns_per_s);
    try testing.expectEqual(@as(u32, 2), r.in_flight);
    try testing.expectEqual(@as(u32, 0), r.overdue);
    try testing.expectEqual(@as(u64, 2), fuse_ops_inflight.load());
    // Past the floor (no samples in either class ⇒ 5 s): both overdue, two
    // lines plus the census.
    const before = fuse_op_watchdog_overdue.load();
    log.testing.expectErrorLogs(3);
    r = wd.tickOnce(t0 + 6 * std.time.ns_per_s);
    try log.testing.expectAllFired();
    try testing.expectEqual(@as(u32, 2), r.overdue);
    try testing.expectEqual(before + 2, fuse_op_watchdog_overdue.load());
    // A second scan observes them again (one observation per scan per stuck op).
    log.testing.expectErrorLogs(3);
    _ = wd.tickOnce(t0 + 11 * std.time.ns_per_s);
    try log.testing.expectAllFired();
    try testing.expectEqual(before + 4, fuse_op_watchdog_overdue.load());
    // Released: gone from the next scan.
    reg.release(a);
    reg.release(far);
    r = wd.tickOnce(t0 + 16 * std.time.ns_per_s);
    try testing.expectEqual(@as(u32, 0), r.in_flight);
    try testing.expectEqual(@as(u32, 0), r.overdue);
    try testing.expectEqual(@as(u64, 4), wd.ticks.load(.monotonic));
}

test "the watchdog never cancels: a slot that stays claimed across many ticks is reported every time and its state word is never touched" {
    var reg = try Registry.init(testing.allocator, 1);
    defer reg.deinit();
    var wd = Watchdog.init(&reg);
    const t0: u64 = 50 * std.time.ns_per_s;
    reg.refreshCoarse(t0);
    const s = reg.claim(.fsync, 5, 55).?;
    var i: u64 = 0;
    while (i < 5) : (i += 1) {
        log.testing.expectErrorLogs(2);
        _ = wd.tickOnce(t0 + (6 + 5 * i) * std.time.ns_per_s);
        try log.testing.expectAllFired();
        try testing.expectEqual(@as(u32, 1), s.state.load(.acquire));
    }
    reg.release(s);
}

test "the watchdog loop runs on an Io and stops at the next tick" {
    var reg = try Registry.init(testing.allocator, 1);
    defer reg.deinit();
    var wd = Watchdog.init(&reg);
    wd.requestStop();
    // Stopped before the first sleep: returns without a tick.
    wd.run(testing.io);
    try testing.expectEqual(@as(u64, 0), wd.ticks.load(.monotonic));
}
