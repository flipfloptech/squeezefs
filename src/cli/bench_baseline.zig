//! The committed microbench baseline and its regression thresholds (charter
//! §11 "a committed microbench baseline with per-group regression
//! thresholds"; the measurement law). Pure: the folding, the statistics,
//! the thresholds, the verdict and the JSON codecs run on every host; the
//! rig that drives the binary is `cli/bench_baseline_rig.zig` (`zig build
//! bench-baseline` / `bench-compare`).
//!
//! GROUPS are the six workloads of `squeezefs bench` (write seq 1 MiB, read
//! seq 1 MiB, read rand 4 KiB, write rand 4 KiB, stat, del), each keyed from
//! a row's `pass`/`block`/`access`, plus the four shapes of the hash
//! microbench (`bench --hash`, decision 0003 §M: the `sqzhash64` column —
//! the low-64 projection every 8-byte format field pays — at 16 B, 100 B,
//! 4 KiB, 256 KiB; hashes/s is the group's ops/s; the xxh3-64 reference and
//! the full 128-bit columns ride in the rows, unjudged).
//! A RUN folds the rows of one `bench` invocation per group: ops/s over the
//! group's total elapsed, the median of the rows' p50 and the max of the
//! rows' p99 (the bucket upper bounds the bench reports — never a midpoint;
//! a hash cell has no histogram and reports 0), plus the admission and
//! reclaim ledgers summed. A BASELINE is `runs` RUNS (3 by default) of the same
//! shape on one venue: per group the mean and the sample standard deviation
//! of ops/s and p99, and the measurement-law fields — instrument (train,
//! commit, profile), substrate, venue (kernel, cpu, disk class), label
//! (`scoping-venue` on a dev box: the charter's acceptance brackets run on
//! the quiet reference box only).
//!
//! THRESHOLDS derive from the baseline's own measured variance: a group may
//! lose at most `max(3σ, 5 %)` of its mean ops/s and gain at most
//! `max(3σ, 5 %)` on its mean p99 before `bench-compare` fails — 3σ so a
//! noisy venue judges itself leniently and a quiet one tightly, 5 % as the
//! floor below which a difference is not a regression anyone can act on.
//! Thresholds are committed beside the baseline (`bench/thresholds-<venue
//! id>.json`); the VENUE ID (kernel release + cpu model + disk class) keys
//! both files, so a laptop baseline never judges the reference box: a
//! compare on a venue without its own baseline SKIPS (`no_baseline`).

const std = @import("std");
const Allocator = std.mem.Allocator;
const json_scan = @import("../core/json_scan.zig");
const fuzzkit = @import("../core/fuzzkit.zig");

pub const Group = enum {
    write_seq_1m,
    read_seq_1m,
    read_rand_4k,
    write_rand_4k,
    stat,
    del,
    hash_16b,
    hash_100b,
    hash_4k,
    hash_256k,

    pub const count = @typeInfo(Group).@"enum".field_names.len;

    /// The hash microbench's judged column: the low-64 projection, what
    /// every 8-byte format field pays (`checksum.hash64`).
    pub const judged_hash_algo = "sqzhash64";

    /// From a hash row's `algo` and `block`: the product's column at one of
    /// the four shapes; the reference and full-128 columns are null (unjudged).
    pub fn ofHashRow(algo: []const u8, block: u64) ?Group {
        if (!std.mem.eql(u8, algo, judged_hash_algo)) return null;
        return switch (block) {
            16 => .hash_16b,
            100 => .hash_100b,
            4096 => .hash_4k,
            262_144 => .hash_256k,
            else => null,
        };
    }

    pub fn isHash(self: Group) bool {
        return switch (self) {
            .hash_16b, .hash_100b, .hash_4k, .hash_256k => true,
            else => false,
        };
    }

    /// From a row's `pass`, `block` and `access` (the bench's JSON).
    pub fn ofRow(pass: []const u8, block: u64, rand: bool) ?Group {
        if (std.mem.eql(u8, pass, "stat")) return .stat;
        if (std.mem.eql(u8, pass, "del")) return .del;
        const seq_block: u64 = 1 << 20;
        const rand_block: u64 = 4096;
        if (std.mem.eql(u8, pass, "write")) return if (rand and block == rand_block) .write_rand_4k else if (!rand and block == seq_block) .write_seq_1m else null;
        if (std.mem.eql(u8, pass, "read")) return if (rand and block == rand_block) .read_rand_4k else if (!rand and block == seq_block) .read_seq_1m else null;
        return null;
    }
};

/// One group's fold over one run.
pub const RunGroup = struct {
    ops: u64 = 0,
    elapsed_ns: u64 = 0,
    bytes: u64 = 0,
    /// Median of the rows' p50 (bucket upper bounds, ns).
    p50_ns: u64 = 0,
    /// Max of the rows' p99.
    p99_ns: u64 = 0,
    admission_bytes: u64 = 0,
    terminal_frees: u64 = 0,
    reclaim_queued: u64 = 0,
    reclaim_elided: u64 = 0,
    discard_bytes: u64 = 0,
    rows: u32 = 0,

    pub fn opsPerS(self: RunGroup) f64 {
        if (self.elapsed_ns == 0) return 0;
        return @as(f64, @floatFromInt(self.ops)) * std.time.ns_per_s / @as(f64, @floatFromInt(self.elapsed_ns));
    }
};

pub const Run = struct {
    groups: [Group.count]RunGroup = @splat(.{}),
    /// Every byte-moving row carried at least `MIN_ROW_OPS` samples; false ⇒
    /// the shape is too small for this venue and the run is not baseline
    /// material.
    rows_long_enough: bool = true,

    pub fn group(self: *const Run, g: Group) *const RunGroup {
        return &self.groups[@backingInt(g)];
    }
};

/// The row minimum on a scoping venue: a byte-moving row must hold at least
/// 256 ops — the sample count below which a p99 is the max of a handful and
/// an ops/s is one burst (the measurement law's 60 s sustained floor is the
/// reference box's headline law; a dev venue's baseline is scoping only and
/// says so in its label).
pub const MIN_ROW_OPS: u64 = 256;

/// Fold the JSON lines of one `squeezefs bench --json` invocation.
pub fn foldRun(gpa: Allocator, json_lines: []const u8) !Run {
    var run: Run = .{};
    var p50s: [Group.count]std.ArrayList(u64) = @splat(.empty);
    defer for (&p50s) |*l| l.deinit(gpa);
    var lines = std.mem.tokenizeScalar(u8, json_lines, '\n');
    while (lines.next()) |line| {
        if (!std.mem.startsWith(u8, line, "{\"pass\":\"")) continue;
        const pass = json_scan.string(line, "pass") orelse continue;
        const block = json_scan.number(line, "block") orelse continue;
        const access = json_scan.string(line, "access") orelse continue;
        // A hash row: the throughput mode (`access` = `cpu`) of the judged column; the chained rows are evidence, not a group.
        const g = (if (std.mem.eql(u8, pass, "hash")) (if (std.mem.eql(u8, access, "cpu")) Group.ofHashRow(json_scan.string(line, "algo") orelse continue, block) else null) else Group.ofRow(pass, block, std.mem.eql(u8, access, "rand"))) orelse continue;
        const rg = &run.groups[@backingInt(g)];
        const elapsed = json_scan.number(line, "elapsed_ns") orelse 0;
        rg.ops += json_scan.number(line, "ops") orelse 0;
        rg.elapsed_ns += elapsed;
        rg.bytes += json_scan.number(line, "bytes") orelse 0;
        rg.admission_bytes += json_scan.number(line, "admission_bytes") orelse 0;
        rg.terminal_frees += json_scan.number(line, "terminal_frees") orelse 0;
        rg.reclaim_queued += json_scan.number(line, "queued") orelse 0;
        rg.reclaim_elided += json_scan.number(line, "elided") orelse 0;
        rg.discard_bytes += json_scan.number(line, "discard_bytes") orelse 0;
        rg.p99_ns = @max(rg.p99_ns, json_scan.number(line, "p99") orelse 0);
        try p50s[@backingInt(g)].append(gpa, json_scan.number(line, "p50") orelse 0);
        rg.rows += 1;
        const moves_bytes = g != .stat and g != .del and !g.isHash();
        if (moves_bytes and (json_scan.number(line, "ops") orelse 0) < MIN_ROW_OPS) run.rows_long_enough = false;
    }
    for (&run.groups, 0..) |*rg, i| {
        const l = p50s[i].items;
        if (l.len == 0) continue;
        std.mem.sort(u64, l, {}, std.sort.asc(u64));
        rg.p50_ns = l[l.len / 2];
    }
    return run;
}

pub const Stat = struct {
    mean: f64,
    /// Sample standard deviation (n − 1); 0 for one run.
    sigma: f64,

    pub fn of(values: []const f64) Stat {
        if (values.len == 0) return .{ .mean = 0, .sigma = 0 };
        var sum: f64 = 0;
        for (values) |v| sum += v;
        const mean = sum / @as(f64, @floatFromInt(values.len));
        if (values.len < 2) return .{ .mean = mean, .sigma = 0 };
        var ss: f64 = 0;
        for (values) |v| ss += (v - mean) * (v - mean);
        return .{ .mean = mean, .sigma = @sqrt(ss / @as(f64, @floatFromInt(values.len - 1))) };
    }
};

pub const GroupStats = struct {
    ops_per_s: Stat,
    p50_ns: Stat,
    p99_ns: Stat,
    /// The last run's ledgers (the measurement law's write-row fields).
    bytes: u64,
    admission_bytes: u64,
    terminal_frees: u64,
    reclaim_queued: u64,
    reclaim_elided: u64,
    discard_bytes: u64,
};

pub const Venue = struct {
    id: []const u8,
    kernel: []const u8,
    cpu: []const u8,
    disk_class: []const u8,
};

pub const Instrument = struct {
    train: []const u8,
    commit: []const u8,
    profile: []const u8,
};

pub const Shape = struct {
    threads: u32,
    files: u32,
    size: u64,
    iterations: u32,
    /// `--time`: every pass's box in seconds (0 = full coverage).
    time_box_s: u32,
};

pub const Baseline = struct {
    schema: u32 = SCHEMA,
    label: []const u8,
    venue: Venue,
    instrument: Instrument,
    substrate: []const u8,
    shape: Shape,
    runs: u32,
    unix_time: u64,
    groups: [Group.count]GroupStats,

    /// Forward-only: a foreign schema refuses the compare (never a silent
    /// reinterpretation of another layout's numbers).
    pub const SCHEMA: u32 = 1;
};

pub const GroupThreshold = struct {
    /// The largest ops/s drop below the baseline mean that passes.
    ops_per_s_drop_max: f64,
    /// The largest p99 rise above the baseline mean that passes (ns).
    p99_ns_rise_max: f64,
};

pub const Thresholds = struct {
    schema: u32 = Baseline.SCHEMA,
    venue_id: []const u8,
    /// The law the numbers came from, restated so a reader of the file
    /// needs no source.
    law: []const u8 = "max(3 sigma, 5 % of the mean) per group, from the baseline's runs",
    groups: [Group.count]GroupThreshold,
};

/// The threshold law: `max(3σ, 5 % × mean)`.
pub fn thresholdOf(s: Stat) f64 {
    return @max(THRESHOLD_SIGMAS * s.sigma, THRESHOLD_FLOOR_PCT * s.mean / 100.0);
}

/// 3σ: a venue's own noise, read off its runs, judges it; 5 %: the floor
/// under which a difference is below what anyone acts on.
pub const THRESHOLD_SIGMAS: f64 = 3.0;
pub const THRESHOLD_FLOOR_PCT: f64 = 5.0;

pub fn aggregate(runs: []const Run, label: []const u8, venue: Venue, instrument: Instrument, substrate: []const u8, shape: Shape, unix_time: u64) struct { baseline: Baseline, thresholds: Thresholds } {
    var groups: [Group.count]GroupStats = undefined;
    var th: [Group.count]GroupThreshold = undefined;
    // 3 runs is the default; the arrays size for a generous campaign.
    var ops: [max_runs]f64 = undefined;
    var p50: [max_runs]f64 = undefined;
    var p99: [max_runs]f64 = undefined;
    const n = @min(runs.len, max_runs);
    for (0..Group.count) |gi| {
        for (runs[0..n], 0..) |r, ri| {
            ops[ri] = r.groups[gi].opsPerS();
            p50[ri] = @floatFromInt(r.groups[gi].p50_ns);
            p99[ri] = @floatFromInt(r.groups[gi].p99_ns);
        }
        const last = runs[n - 1].groups[gi];
        groups[gi] = .{
            .ops_per_s = Stat.of(ops[0..n]),
            .p50_ns = Stat.of(p50[0..n]),
            .p99_ns = Stat.of(p99[0..n]),
            .bytes = last.bytes,
            .admission_bytes = last.admission_bytes,
            .terminal_frees = last.terminal_frees,
            .reclaim_queued = last.reclaim_queued,
            .reclaim_elided = last.reclaim_elided,
            .discard_bytes = last.discard_bytes,
        };
        th[gi] = .{ .ops_per_s_drop_max = thresholdOf(groups[gi].ops_per_s), .p99_ns_rise_max = thresholdOf(groups[gi].p99_ns) };
    }
    return .{
        .baseline = .{ .label = label, .venue = venue, .instrument = instrument, .substrate = substrate, .shape = shape, .runs = @intCast(n), .unix_time = unix_time, .groups = groups },
        .thresholds = .{ .venue_id = venue.id, .groups = th },
    };
}

/// Runs beyond this are not folded (a baseline is 3 runs; 16 leaves room
/// for a longer calibration without a heap allocation).
pub const max_runs: usize = 16;

pub const Verdict = struct {
    group: Group,
    ops_per_s: f64,
    baseline_ops_per_s: f64,
    ops_regressed: bool,
    p99_ns: f64,
    baseline_p99_ns: f64,
    p99_regressed: bool,

    pub fn regressed(self: Verdict) bool {
        return self.ops_regressed or self.p99_regressed;
    }
};

/// Judge one run against the baseline under the thresholds.
pub fn judge(baseline: *const Baseline, thresholds: *const Thresholds, run: *const Run) [Group.count]Verdict {
    var out: [Group.count]Verdict = undefined;
    for (0..Group.count) |gi| {
        const b = baseline.groups[gi];
        const t = thresholds.groups[gi];
        const ops = run.groups[gi].opsPerS();
        const p99: f64 = @floatFromInt(run.groups[gi].p99_ns);
        out[gi] = .{
            .group = @fromBackingInt(@intCast(gi)),
            .ops_per_s = ops,
            .baseline_ops_per_s = b.ops_per_s.mean,
            .ops_regressed = ops < b.ops_per_s.mean - t.ops_per_s_drop_max,
            .p99_ns = p99,
            .baseline_p99_ns = b.p99_ns.mean,
            .p99_regressed = p99 > b.p99_ns.mean + t.p99_ns_rise_max,
        };
    }
    return out;
}

/// `<kernel>__<cpu>__<disk class>`, each part lower-cased with runs of
/// anything but alphanumerics, `.` and `-` collapsed to `-`: a file name
/// and a key.
pub fn venueId(gpa: Allocator, kernel: []const u8, cpu: []const u8, disk_class: []const u8) ![]u8 {
    var out: std.ArrayList(u8) = .empty;
    errdefer out.deinit(gpa);
    for ([_][]const u8{ kernel, cpu, disk_class }, 0..) |part, i| {
        if (i != 0) try out.appendSlice(gpa, "__");
        var dash = false;
        for (std.mem.trim(u8, part, " \t\r\n")) |c| {
            if (std.ascii.isAlphanumeric(c) or c == '.' or c == '-') {
                try out.append(gpa, std.ascii.toLower(c));
                dash = false;
            } else if (!dash) {
                try out.append(gpa, '-');
                dash = true;
            }
        }
    }
    return out.toOwnedSlice(gpa);
}

/// The cpu model off `/proc/cpuinfo` text: `model name` where it exists
/// (x86), else `CPU implementer`/`CPU part` (arm64), plus the core count.
pub fn cpuModel(gpa: Allocator, cpuinfo: []const u8, cpus: u32) ![]u8 {
    var model: []const u8 = "unknown";
    var implementer: []const u8 = "";
    var part: []const u8 = "";
    var lines = std.mem.splitScalar(u8, cpuinfo, '\n');
    while (lines.next()) |line| {
        const colon = std.mem.indexOfScalar(u8, line, ':') orelse continue;
        const key = std.mem.trim(u8, line[0..colon], " \t");
        const val = std.mem.trim(u8, line[colon + 1 ..], " \t\r");
        if (std.mem.eql(u8, key, "model name") and std.mem.eql(u8, model, "unknown")) model = val;
        if (std.mem.eql(u8, key, "CPU implementer") and implementer.len == 0) implementer = val;
        if (std.mem.eql(u8, key, "CPU part") and part.len == 0) part = val;
    }
    if (std.mem.eql(u8, model, "unknown") and implementer.len != 0) return std.fmt.allocPrint(gpa, "arm-impl{s}-part{s}-{d}c", .{ implementer, part, cpus });
    return std.fmt.allocPrint(gpa, "{s}-{d}c", .{ model, cpus });
}

pub fn writeJson(gpa: Allocator, v: anytype) ![]u8 {
    return std.json.Stringify.valueAlloc(gpa, v, .{ .whitespace = .indent_2 });
}

pub fn parseBaseline(gpa: Allocator, text: []const u8) !std.json.Parsed(Baseline) {
    const p = try std.json.parseFromSlice(Baseline, gpa, text, .{});
    errdefer p.deinit();
    if (p.value.schema != Baseline.SCHEMA) return error.ForeignSchema;
    return p;
}

pub fn parseThresholds(gpa: Allocator, text: []const u8) !std.json.Parsed(Thresholds) {
    const p = try std.json.parseFromSlice(Thresholds, gpa, text, .{});
    errdefer p.deinit();
    if (p.value.schema != Baseline.SCHEMA) return error.ForeignSchema;
    return p;
}

pub fn baselineFileName(gpa: Allocator, venue_id: []const u8) ![]u8 {
    return std.fmt.allocPrint(gpa, "baseline-{s}.json", .{venue_id});
}

pub fn thresholdsFileName(gpa: Allocator, venue_id: []const u8) ![]u8 {
    return std.fmt.allocPrint(gpa, "thresholds-{s}.json", .{venue_id});
}

const testing = std.testing;

fn baselineJsonFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [4096]u8 = undefined;
    const len = smith.slice(&buf);
    const text = buf[0..len];
    if (parseBaseline(testing.allocator, text)) |*p| {
        var pp = p.*;
        defer pp.deinit();
        // What parses re-serializes and parses again.
        const again = try writeJson(testing.allocator, pp.value);
        defer testing.allocator.free(again);
        var p2 = try parseBaseline(testing.allocator, again);
        p2.deinit();
    } else |_| {}
    if (parseThresholds(testing.allocator, text)) |*p| {
        var pp = p.*;
        pp.deinit();
    } else |_| {}
}

test "fuzz: the baseline and thresholds JSON decoders are total on arbitrary bytes; a baseline that parses re-serializes to a parseable baseline" {
    try fuzzkit.fuzz({}, baselineJsonFuzz, .{ .corpus = &.{ fuzzkit.slice("{\"schema\":1}"), fuzzkit.slice("{\"schema\":2,\"venue_id\":\"x\",\"groups\":[]}") } });
}

const sample_rows =
    \\{"pass":"write","arm":"","iteration":1,"block":1048576,"access":"seq","io":"buffered","threads":2,"files":1,"size":536870912,"elapsed_ns":1200000000,"ops":1024,"expected_ops":1024,"boxed_out":false,"bytes":1073741824,"mib_per_s":853,"iops":853,"scoping_only":true,"lat_ns":{"min_bucket":1024,"mean":1100000,"p50":1048576,"p99":4194304,"p999":8388608,"max_bucket":8388608},"stats_present":true,"user_bytes":1073741824,"device_write_bytes":1073741824,"device_writes":256,"amplification_ppm":1000000,"wareq_sz":4194304,"admission_bytes":1073741824,"reclaim":{"terminal_frees":3,"queued":2,"elided":1,"discard_bytes":4096},"fuse_ops":1024,"transport_ring_requests":1024,"transport_enters":1024,"engagement":{"held_stores":0,"patch_writes":0,"hot_block_hits":0,"fast_dispatch_serves":0,"zc_write_direct_bytes":0,"zc_write_extract_bytes":0},"invariant_tripwires":0}
    \\{"pass":"read","arm":"","iteration":1,"block":4096,"access":"rand","io":"buffered","threads":2,"files":1,"size":536870912,"elapsed_ns":2000000000,"ops":100000,"expected_ops":262144,"boxed_out":true,"bytes":409600000,"mib_per_s":195,"iops":50000,"scoping_only":true,"lat_ns":{"min_bucket":1024,"mean":20000,"p50":16384,"p99":65536,"p999":131072,"max_bucket":262144},"stats_present":true,"user_bytes":409600000,"device_write_bytes":0,"device_writes":0,"amplification_ppm":0,"wareq_sz":0,"admission_bytes":0,"reclaim":{"terminal_frees":0,"queued":0,"elided":0,"discard_bytes":0},"fuse_ops":100000,"transport_ring_requests":100000,"transport_enters":100000,"engagement":{"held_stores":0,"patch_writes":0,"hot_block_hits":0,"fast_dispatch_serves":0,"zc_write_direct_bytes":0,"zc_write_extract_bytes":0},"invariant_tripwires":0}
    \\{"pass":"read","arm":"","iteration":2,"block":4096,"access":"rand","io":"buffered","threads":2,"files":1,"size":536870912,"elapsed_ns":2000000000,"ops":120000,"expected_ops":262144,"boxed_out":true,"bytes":491520000,"mib_per_s":234,"iops":60000,"scoping_only":true,"lat_ns":{"min_bucket":1024,"mean":18000,"p50":32768,"p99":32768,"p999":131072,"max_bucket":262144},"stats_present":true,"user_bytes":491520000,"device_write_bytes":0,"device_writes":0,"amplification_ppm":0,"wareq_sz":0,"admission_bytes":0,"reclaim":{"terminal_frees":0,"queued":0,"elided":0,"discard_bytes":0},"fuse_ops":120000,"transport_ring_requests":120000,"transport_enters":120000,"engagement":{"held_stores":0,"patch_writes":0,"hot_block_hits":0,"fast_dispatch_serves":0,"zc_write_direct_bytes":0,"zc_write_extract_bytes":0},"invariant_tripwires":0}
    \\{"pass":"stat","arm":"","iteration":1,"block":1048576,"access":"seq","io":"buffered","threads":2,"files":1,"size":536870912,"elapsed_ns":200000,"ops":2,"expected_ops":2,"boxed_out":false,"bytes":0,"mib_per_s":0,"iops":10000,"scoping_only":false,"lat_ns":{"min_bucket":1024,"mean":50000,"p50":65536,"p99":131072,"p999":131072,"max_bucket":131072},"stats_present":true,"user_bytes":0,"device_write_bytes":0,"device_writes":0,"amplification_ppm":0,"wareq_sz":0,"admission_bytes":0,"reclaim":{"terminal_frees":0,"queued":0,"elided":0,"discard_bytes":0},"fuse_ops":2,"transport_ring_requests":2,"transport_enters":2,"engagement":{"held_stores":0,"patch_writes":0,"hot_block_hits":0,"fast_dispatch_serves":0,"zc_write_direct_bytes":0,"zc_write_extract_bytes":0},"invariant_tripwires":0}
    \\
;

test "fold: rows key into their groups; ops/s over the group's total elapsed; p50 is the median of the rows' p50, p99 the max; the ledgers sum; a byte-moving row under the sample floor flags the run" {
    const run = try foldRun(testing.allocator, sample_rows);
    const w = run.group(.write_seq_1m);
    try testing.expectEqual(@as(u64, 1024), w.ops);
    try testing.expectApproxEqAbs(@as(f64, 853.33), w.opsPerS(), 0.01);
    try testing.expectEqual(@as(u64, 1073741824), w.admission_bytes);
    try testing.expectEqual(@as(u64, 3), w.terminal_frees);
    try testing.expectEqual(@as(u64, 4096), w.discard_bytes);
    const r = run.group(.read_rand_4k);
    try testing.expectEqual(@as(u32, 2), r.rows);
    try testing.expectEqual(@as(u64, 220000), r.ops);
    try testing.expectApproxEqAbs(@as(f64, 55000), r.opsPerS(), 0.01);
    try testing.expectEqual(@as(u64, 32768), r.p50_ns);
    try testing.expectEqual(@as(u64, 65536), r.p99_ns);
    try testing.expectEqual(@as(u32, 0), run.group(.del).rows);
    try testing.expect(run.rows_long_enough);
    const short = try foldRun(testing.allocator, "{\"pass\":\"write\",\"block\":1048576,\"access\":\"seq\",\"elapsed_ns\":5000000000,\"ops\":255,\"bytes\":1,\"p50\":1,\"p99\":1}\n");
    try testing.expect(!short.rows_long_enough);
}

test "Group.ofHashRow: the sqzhash64 column keys the four hash groups by shape; the reference and full-128 columns and a foreign shape are null; the fold reads a hash row's throughput mode only" {
    try testing.expectEqual(Group.hash_16b, Group.ofHashRow("sqzhash64", 16).?);
    try testing.expectEqual(Group.hash_100b, Group.ofHashRow("sqzhash64", 100).?);
    try testing.expectEqual(Group.hash_4k, Group.ofHashRow("sqzhash64", 4096).?);
    try testing.expectEqual(Group.hash_256k, Group.ofHashRow("sqzhash64", 262_144).?);
    try testing.expectEqual(@as(?Group, null), Group.ofHashRow("xxh3-64", 16));
    try testing.expectEqual(@as(?Group, null), Group.ofHashRow("sqzhash128", 16));
    try testing.expectEqual(@as(?Group, null), Group.ofHashRow("sqzhash64", 17));
    try testing.expect(Group.hash_4k.isHash() and !Group.stat.isHash());
    const run = try foldRun(testing.allocator, "{\"pass\":\"hash\",\"algo\":\"sqzhash64\",\"block\":4096,\"access\":\"cpu\",\"ops\":1000000,\"elapsed_ns\":250000000,\"bytes\":4096000000,\"ps_per_op\":250000,\"mb_per_s\":16384}\n{\"pass\":\"hash\",\"algo\":\"xxh3-64\",\"block\":4096,\"access\":\"cpu\",\"ops\":1,\"elapsed_ns\":1,\"bytes\":4096,\"ps_per_op\":1,\"mb_per_s\":1}\n{\"pass\":\"hash\",\"algo\":\"sqzhash64\",\"block\":4096,\"access\":\"chained\",\"ops\":1,\"elapsed_ns\":1,\"bytes\":4096,\"ps_per_op\":1,\"mb_per_s\":1}\n");
    const g = run.group(.hash_4k);
    try testing.expectEqual(@as(u32, 1), g.rows);
    try testing.expectEqual(@as(u64, 1_000_000), g.ops);
    try testing.expectApproxEqAbs(@as(f64, 4_000_000), g.opsPerS(), 0.01);
    try testing.expectEqual(@as(u64, 0), g.p99_ns);
    try testing.expect(run.rows_long_enough);
}

test "Group.ofRow: the six workloads; a foreign block or access is null" {
    try testing.expectEqual(Group.write_seq_1m, Group.ofRow("write", 1 << 20, false).?);
    try testing.expectEqual(Group.read_rand_4k, Group.ofRow("read", 4096, true).?);
    try testing.expectEqual(Group.write_rand_4k, Group.ofRow("write", 4096, true).?);
    try testing.expectEqual(Group.stat, Group.ofRow("stat", 0, false).?);
    try testing.expectEqual(@as(?Group, null), Group.ofRow("write", 8192, true));
    try testing.expectEqual(@as(?Group, null), Group.ofRow("read", 1 << 20, true));
}

test "the threshold law: max(3 sigma, 5 %) — a quiet venue judges at 5 %, a noisy one at 3 sigma; the verdict fails a drop past the floor and passes one inside it" {
    try testing.expectApproxEqAbs(@as(f64, 50), thresholdOf(.{ .mean = 1000, .sigma = 10 }), 0.001);
    try testing.expectApproxEqAbs(@as(f64, 300), thresholdOf(.{ .mean = 1000, .sigma = 100 }), 0.001);
    const s = Stat.of(&.{ 100, 110, 120 });
    try testing.expectApproxEqAbs(@as(f64, 110), s.mean, 0.001);
    try testing.expectApproxEqAbs(@as(f64, 10), s.sigma, 0.001);
    var runs: [3]Run = @splat(.{});
    for (&runs, 0..) |*r, i| {
        r.groups[@backingInt(Group.read_rand_4k)] = .{ .ops = 1000 + 10 * @as(u64, @intCast(i)), .elapsed_ns = std.time.ns_per_s, .p99_ns = 1000 };
    }
    const agg = aggregate(&runs, "scoping-venue", .{ .id = "v", .kernel = "k", .cpu = "c", .disk_class = "file" }, .{ .train = "0.1.0", .commit = "abc", .profile = "gate" }, "file", .{ .threads = 2, .files = 1, .size = 1, .iterations = 1, .time_box_s = 5 }, 0);
    const gi = @backingInt(Group.read_rand_4k);
    try testing.expectApproxEqAbs(@as(f64, 1010), agg.baseline.groups[gi].ops_per_s.mean, 0.001);
    // 3σ = 30 < 5 % = 50.5 → the floor.
    try testing.expectApproxEqAbs(@as(f64, 50.5), agg.thresholds.groups[gi].ops_per_s_drop_max, 0.001);
    var ok: Run = .{};
    ok.groups[gi] = .{ .ops = 965, .elapsed_ns = std.time.ns_per_s, .p99_ns = 1000 };
    var bad: Run = .{};
    bad.groups[gi] = .{ .ops = 955, .elapsed_ns = std.time.ns_per_s, .p99_ns = 1000 };
    var slow: Run = .{};
    slow.groups[gi] = .{ .ops = 1010, .elapsed_ns = std.time.ns_per_s, .p99_ns = 1051 };
    try testing.expect(!judge(&agg.baseline, &agg.thresholds, &ok)[gi].regressed());
    try testing.expect(judge(&agg.baseline, &agg.thresholds, &bad)[gi].ops_regressed);
    try testing.expect(judge(&agg.baseline, &agg.thresholds, &slow)[gi].p99_regressed);
}

test "the JSON codecs round-trip a baseline and its thresholds; a foreign schema refuses" {
    const gpa = testing.allocator;
    var runs: [2]Run = @splat(.{});
    runs[0].groups[0] = .{ .ops = 10, .elapsed_ns = std.time.ns_per_s, .p50_ns = 5, .p99_ns = 9, .bytes = 77 };
    runs[1].groups[0] = .{ .ops = 12, .elapsed_ns = std.time.ns_per_s, .p50_ns = 5, .p99_ns = 11, .bytes = 78 };
    const agg = aggregate(&runs, "scoping-venue", .{ .id = "k__c__d", .kernel = "k", .cpu = "c", .disk_class = "d" }, .{ .train = "0.1.0", .commit = "abc", .profile = "gate" }, "file", .{ .threads = 2, .files = 1, .size = 4, .iterations = 2, .time_box_s = 5 }, 42);
    const bj = try writeJson(gpa, agg.baseline);
    defer gpa.free(bj);
    const tj = try writeJson(gpa, agg.thresholds);
    defer gpa.free(tj);
    var pb = try parseBaseline(gpa, bj);
    defer pb.deinit();
    var pt = try parseThresholds(gpa, tj);
    defer pt.deinit();
    try testing.expectEqualStrings("k__c__d", pb.value.venue.id);
    try testing.expectEqual(@as(u32, 2), pb.value.runs);
    try testing.expectApproxEqAbs(@as(f64, 11), pb.value.groups[0].ops_per_s.mean, 0.001);
    try testing.expectEqual(@as(u64, 78), pb.value.groups[0].bytes);
    try testing.expectApproxEqAbs(agg.thresholds.groups[0].p99_ns_rise_max, pt.value.groups[0].p99_ns_rise_max, 0.001);
    var foreign = agg.baseline;
    foreign.schema = Baseline.SCHEMA + 1;
    const fj = try writeJson(gpa, foreign);
    defer gpa.free(fj);
    try testing.expectError(error.ForeignSchema, parseBaseline(gpa, fj));
}

test "the venue id and the cpu model: lower-cased, collapsed, keyed; arm64 falls back to implementer/part" {
    const gpa = testing.allocator;
    const id = try venueId(gpa, "7.2.6-sqz", "Intel(R) Xeon(R) Gold 6338 CPU @ 2.00GHz-32c", "file-ext4");
    defer gpa.free(id);
    try testing.expectEqualStrings("7.2.6-sqz__intel-r-xeon-r-gold-6338-cpu-2.00ghz-32c__file-ext4", id);
    const x86 = try cpuModel(gpa, "processor\t: 0\nmodel name\t: Intel(R) Xeon(R) Gold 6338 CPU @ 2.00GHz\n", 32);
    defer gpa.free(x86);
    try testing.expectEqualStrings("Intel(R) Xeon(R) Gold 6338 CPU @ 2.00GHz-32c", x86);
    const arm = try cpuModel(gpa, "processor\t: 0\nCPU implementer\t: 0x61\nCPU part\t: 0x000\n", 9);
    defer gpa.free(arm);
    try testing.expectEqualStrings("arm-impl0x61-part0x000-9c", arm);
}
