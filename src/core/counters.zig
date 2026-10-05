//! Observability primitives (charter §10): per-thread sharded counters folded
//! on read, exact-sum latency histograms, and the must-stay-0 invariant
//! tripwire.
//!
//! ONE law for every sharded instrument: each thread owns a stripe chosen once
//! (round-robin at first use, cached thread-locally — never `sched_getcpu` per
//! op; a migrating thread keeps its stripe), increments are Relaxed RMWs on a
//! core-local cache line, and a read FOLDS every stripe. Addition commutes, so
//! the fold is exact for everything that happened-before the read; a
//! concurrent increment may or may not be included, exactly as with a single
//! atomic. (The reference replaced a per-thread batching counter whose
//! invisible residue manufactured a 2× ghost in stats deltas — exactness is the
//! requirement; `docs/porting/knobs_and_rails.md` §4.)

const std = @import("std");
const derive = @import("derive.zig");
const clock = @import("clock.zig");
const log = @import("log.zig");

/// Stripe count: `clamp(next_pow2(cpus), 1, 64)` — derived from the machine,
/// capped at 64 because above 64 live threads stripes are shared and still
/// exact; a wider table only costs cache (64 × 64 B = 4 KiB per counter).
pub const max_stripes = 64;

pub fn stripeCount(cpus: u32) usize {
    return @intCast(derive.clamp(derive.nextPow2(cpus), 1, max_stripes));
}

var stripe_count_cache: std.atomic.Value(usize) = .init(0);

fn stripeCountLive() usize {
    const cached = stripe_count_cache.load(.monotonic);
    if (cached != 0) return cached;
    const n = stripeCount(@intCast(@max(1, std.Thread.getCpuCount() catch 1)));
    stripe_count_cache.store(n, .monotonic);
    return n;
}

var next_stripe: std.atomic.Value(usize) = .init(0);
threadlocal var my_stripe: usize = std.math.maxInt(usize);

/// This thread's stripe, chosen once.
pub fn stripeIndex() usize {
    if (my_stripe == std.math.maxInt(usize)) {
        my_stripe = next_stripe.fetchAdd(1, .monotonic) % stripeCountLive();
    }
    return my_stripe;
}

const Line = struct {
    v: std.atomic.Value(u64) align(std.atomic.cache_line) = .init(0),
};

/// A monotone counter striped per thread. Zero-cost to declare as a global
/// (`pub var x: ShardedCounter = .{}`); the stripes are inline.
pub const ShardedCounter = struct {
    lines: [max_stripes]Line = @splat(.{}),

    pub fn add(self: *ShardedCounter, n: u64) void {
        _ = self.lines[stripeIndex()].v.fetchAdd(n, .monotonic);
    }

    pub fn inc(self: *ShardedCounter) void {
        self.add(1);
    }

    pub fn load(self: *const ShardedCounter) u64 {
        var sum: u64 = 0;
        for (&self.lines) |*l| sum +%= l.v.load(.monotonic);
        return sum;
    }
};

/// A plain gauge: a current value, not a count (set/load; `add`/`sub` for
/// byte ledgers). One cache line so neighbours never false-share.
pub const Gauge = struct {
    v: std.atomic.Value(u64) align(std.atomic.cache_line) = .init(0),

    pub fn set(self: *Gauge, n: u64) void {
        self.v.store(n, .monotonic);
    }
    pub fn add(self: *Gauge, n: u64) void {
        _ = self.v.fetchAdd(n, .monotonic);
    }
    pub fn sub(self: *Gauge, n: u64) void {
        _ = self.v.fetchSub(n, .monotonic);
    }
    pub fn load(self: *const Gauge) u64 {
        return self.v.load(.monotonic);
    }
};

// ---------------------------------------------------------------------------
// Latency histograms: 26 log2 buckets over microseconds, exact count + sum_ns.
// ---------------------------------------------------------------------------

pub const hist_buckets = 26;

pub const bucket_labels = [hist_buckets][]const u8{
    "<=1us", "<=2us", "<=4us", "<=8us", "<=16us", "<=32us", "<=64us", "<=128us", "<=256us", "<=512us",
    "<=1ms", "<=2ms", "<=4ms", "<=8ms", "<=16ms", "<=32ms", "<=64ms", "<=128ms", "<=256ms", "<=512ms",
    "<=1s",  "<=2s",  "<=4s",  "<=8s",  "<=16s",  ">16s",
};

/// `0` if `micros <= 1` else `min(ilog2(micros − 1) + 1, 25)`.
pub fn bucketIndex(micros: u64) usize {
    if (micros <= 1) return 0;
    return @min(@as(usize, std.math.log2_int(u64, micros - 1)) + 1, hist_buckets - 1);
}

pub const LatencyHistogram = struct {
    buckets: [hist_buckets]std.atomic.Value(u64) = @splat(.init(0)),
    count: std.atomic.Value(u64) = .init(0),
    sum_ns: std.atomic.Value(u64) = .init(0),

    /// Three Relaxed RMWs.
    pub fn record(self: *LatencyHistogram, ns: u64) void {
        _ = self.buckets[bucketIndex(ns / 1000)].fetchAdd(1, .monotonic);
        _ = self.count.fetchAdd(1, .monotonic);
        _ = self.sum_ns.fetchAdd(ns, .monotonic);
    }

    pub fn fold(self: *const LatencyHistogram, into: *Snapshot) void {
        for (&self.buckets, 0..) |*b, i| into.buckets[i] +%= b.load(.monotonic);
        into.count +%= self.count.load(.monotonic);
        into.sum_ns +%= self.sum_ns.load(.monotonic);
    }

    pub const Snapshot = struct {
        buckets: [hist_buckets]u64 = @splat(0),
        count: u64 = 0,
        sum_ns: u64 = 0,

        pub fn meanNs(self: Snapshot) u64 {
            return if (self.count == 0) 0 else self.sum_ns / self.count;
        }

        /// `{"buckets": {label: n, …}, "count": c, "sum_ns": s, "mean_ns": m}`
        pub fn writeJson(self: Snapshot, w: *std.Io.Writer) std.Io.Writer.Error!void {
            try w.writeAll("{\"buckets\":{");
            for (bucket_labels, 0..) |label, i| {
                if (i != 0) try w.writeByte(',');
                try w.print("\"{s}\":{d}", .{ label, self.buckets[i] });
            }
            try w.print("}},\"count\":{d},\"sum_ns\":{d},\"mean_ns\":{d}}}", .{ self.count, self.sum_ns, self.meanNs() });
        }
    };
};

const HistStripe = struct {
    h: LatencyHistogram align(256) = .{},
};

/// A per-thread-striped histogram; stripes inline (64 × 256 B = 16 KiB).
pub const ShardedLatencyHistogram = struct {
    stripes: [max_stripes]HistStripe = @splat(.{}),

    pub fn record(self: *ShardedLatencyHistogram, ns: u64) void {
        self.stripes[stripeIndex()].h.record(ns);
    }

    pub fn snapshot(self: *const ShardedLatencyHistogram) LatencyHistogram.Snapshot {
        var s: LatencyHistogram.Snapshot = .{};
        for (&self.stripes) |*st| st.h.fold(&s);
        return s;
    }
};

// ---------------------------------------------------------------------------
// The invariant tripwire (must-stay-0 with a per-site label).
// ---------------------------------------------------------------------------

/// A "cannot happen" that must be loud, counted, and NEVER fatal: a panic in
/// a handler loses a FUSE reply, which is strictly worse than a wrong-but-
/// served op (the `transport_lease_overlong` precedent). Declare one per site:
///
///     pub var lease_overlong: Tripwire = .init("transport_lease_overlong");
///     … lease_overlong.fire("lease held 1.2 s");
///
/// The total rides the stats JSON as `invariant_tripwires` (0 on a healthy
/// daemon); the per-site count is an in-process API tests use to allow-list
/// ONE known routed site while every other stays must-stay-0. Logging is
/// rate-limited to one line per site per second.
pub const Tripwire = struct {
    site: []const u8,
    count: std.atomic.Value(u64) = .init(0),
    last_log_s: std.atomic.Value(u64) = .init(0),
    /// Intrusive lock-free registration list for the stats fold.
    next: ?*Tripwire = null,
    registered: std.atomic.Value(bool) = .init(false),

    pub fn init(site: []const u8) Tripwire {
        return .{ .site = site };
    }

    pub fn fire(self: *Tripwire, detail: []const u8) void {
        _ = self.count.fetchAdd(1, .monotonic);
        _ = tripwire_total.fetchAdd(1, .monotonic);
        self.registerOnce();
        const now = clock.realtimeCoarseS();
        // One log line per site per second: swap in `now`; equal ⇒ quiet.
        if (self.last_log_s.swap(now, .monotonic) != now) {
            log.err("INVARIANT TRIPWIRE '{s}': {s} — a concurrency outcome the design says cannot happen just happened. Counted in invariant_tripwires; the daemon proceeds (a panic here would lose a FUSE reply, which is strictly worse than a wrong-but-served op)", .{ self.site, detail });
        }
    }

    pub fn load(self: *const Tripwire) u64 {
        return self.count.load(.monotonic);
    }

    fn registerOnce(self: *Tripwire) void {
        // Winner of the CAS pushes itself onto the global list. The `next`
        // publication is paired with the acquire load in `iterate`.
        if (self.registered.cmpxchgStrong(false, true, .acq_rel, .monotonic) != null) return;
        var head = tripwire_head.load(.acquire);
        while (true) {
            self.next = head;
            head = tripwire_head.cmpxchgWeak(head, self, .release, .acquire) orelse return;
        }
    }
};

var tripwire_total: std.atomic.Value(u64) = .init(0);
var tripwire_head: std.atomic.Value(?*Tripwire) = .init(null);

/// `invariant_tripwires` — the total across every site.
pub fn invariantTripwires() u64 {
    return tripwire_total.load(.monotonic);
}

/// Per-site count; 0 for a site that never fired.
pub fn tripwireCount(site: []const u8) u64 {
    var cur = tripwire_head.load(.acquire);
    while (cur) |t| : (cur = t.next) {
        if (std.mem.eql(u8, t.site, site)) return t.load();
    }
    return 0;
}

// ---------------------------------------------------------------------------

const testing = std.testing;

test "stripe count derivation: clamp(next_pow2(cpus), 1, 64)" {
    try testing.expectEqual(@as(usize, 1), stripeCount(1));
    try testing.expectEqual(@as(usize, 4), stripeCount(3));
    try testing.expectEqual(@as(usize, 32), stripeCount(32));
    try testing.expectEqual(@as(usize, 64), stripeCount(33));
    try testing.expectEqual(@as(usize, 64), stripeCount(512));
}

test "sharded counter folds exactly across threads" {
    var c: ShardedCounter = .{};
    const per_thread: u64 = 10_000;
    const Worker = struct {
        fn run(cnt: *ShardedCounter) void {
            var i: u64 = 0;
            while (i < per_thread) : (i += 1) cnt.inc();
        }
    };
    var threads: [8]std.Thread = undefined;
    for (&threads) |*t| t.* = try std.Thread.spawn(.{}, Worker.run, .{&c});
    for (threads) |t| t.join();
    try testing.expectEqual(per_thread * threads.len, c.load());
}

test "latency bucket index" {
    try testing.expectEqual(@as(usize, 0), bucketIndex(0));
    try testing.expectEqual(@as(usize, 0), bucketIndex(1));
    try testing.expectEqual(@as(usize, 1), bucketIndex(2));
    try testing.expectEqual(@as(usize, 2), bucketIndex(3));
    try testing.expectEqual(@as(usize, 2), bucketIndex(4));
    try testing.expectEqual(@as(usize, 3), bucketIndex(5));
    try testing.expectEqual(@as(usize, 10), bucketIndex(1000));
    try testing.expectEqual(@as(usize, 25), bucketIndex(17_000_000));
    try testing.expectEqual(@as(usize, 25), bucketIndex(std.math.maxInt(u64)));
}

test "histogram: count and sum_ns are exact; json shape" {
    var h: ShardedLatencyHistogram = .{};
    h.record(500);
    h.record(1_500);
    h.record(3_000_000);
    const s = h.snapshot();
    try testing.expectEqual(@as(u64, 3), s.count);
    try testing.expectEqual(@as(u64, 3_002_000), s.sum_ns);
    try testing.expectEqual(@as(u64, 1_000_666), s.meanNs());
    try testing.expectEqual(@as(u64, 2), s.buckets[0]); // 0 µs and 1 µs
    try testing.expectEqual(@as(u64, 1), s.buckets[bucketIndex(3000)]);
    var buf: [1024]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try s.writeJson(&w);
    try testing.expect(std.mem.startsWith(u8, w.buffered(), "{\"buckets\":{\"<=1us\":2,"));
    try testing.expect(std.mem.endsWith(u8, w.buffered(), ",\"count\":3,\"sum_ns\":3002000,\"mean_ns\":1000666}"));
}

test "tripwire: one fire increments the total and the site by exactly 1, logs loudly; unknown site reads 0" {
    const S = struct {
        var tw: Tripwire = .init("counters_test_site");
    };
    const before = invariantTripwires();
    // Two fires within one second: the first logs, the second is rate-limited.
    log.testing.expectErrorLogs(1);
    S.tw.fire("the test fired it");
    try testing.expectEqual(before + 1, invariantTripwires());
    try testing.expectEqual(@as(u64, 1), tripwireCount("counters_test_site"));
    try testing.expectEqual(@as(u64, 0), tripwireCount("never_fired_site"));
    S.tw.fire("again");
    try testing.expectEqual(@as(u64, 2), S.tw.load());
    try log.testing.expectAllFired();
}
