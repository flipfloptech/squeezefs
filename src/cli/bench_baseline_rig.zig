//! The microbench baseline RIG (charter §11; `bench_baseline.zig` holds the
//! law): `build.zig` hands it the install prefix (it drives the INSTALLED
//! binary, `<prefix>/bin/squeezefs`) and the repo's `bench/` directory
//! through `bench_options` (resolved paths, so the rig is cwd-independent)
//! and runs it as two filtered test steps:
//!
//!   `zig build bench-baseline` — `bench baseline: record`: a format, a
//!   mount, `runs` (3) invocations of `squeezefs bench --json` on the
//!   baseline shape each followed by one `squeezefs bench --hash --json`
//!   (the CPU hash groups, decision 0003 §M), folded per group; writes
//!   `bench/baseline-<venue id>.json` and `bench/thresholds-<venue id>.json`
//!   labelled `scoping-venue`.
//!
//!   `zig build bench-compare` — `bench compare: judge`: one invocation on
//!   the same shape, judged per group against the committed baseline under
//!   the committed thresholds; FAILS on a regression past a threshold; SKIPS
//!   (`no_baseline`) on a venue with no committed baseline of its own — a
//!   laptop's numbers never judge the reference box. Part of `gate-release`,
//!   never of the per-commit gate.
//!
//! The shape: 2 threads × 1 file × 256 MiB at the suite's blocks
//! (O_DIRECT), 2 iterations, every pass time-boxed at 5 s — the sequential
//! passes finish their 512 MiB inside the box (full coverage, ≥ 512 ops),
//! the random passes box out with thousands of ops; the fold refuses a
//! byte-moving row under 256 ops as baseline material. A record is ~90 s.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const bench_options = @import("bench_options");
const baseline = @import("bench_baseline.zig");
const bench_smoke = @import("bench_smoke.zig");
const testkit = @import("../core/testkit.zig");
const json_scan = @import("../core/json_scan.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/bench_baseline_rig is Linux only (charter §1.2)");
}

const testing = std.testing;

const squeezefs_bin = bench_options.install_prefix ++ "/bin/squeezefs";
/// The build steps arm the rig; the unit run (which sees the installed
/// binary too) is not the rig's step.
const rig_knob = "SQUEEZEFS_TEST_BENCH_RIG";

/// The baseline shape (module doc).
const threads: u32 = 2;
const files: u32 = 1;
const file_size: u64 = 256 << 20;
const iterations: u32 = 2;
const time_box_s: u32 = 5;
const runs: u32 = 3;
/// 256 MiB meta + two 2 GiB data volumes: the dataset (512 MiB) plus pack
/// and rewrite headroom; sparse on the venue's ext4.
const meta_len: u64 = 256 << 20;
const data_len: u64 = 2 << 30;

const Facts = struct {
    kernel: []u8,
    cpu: []u8,
    disk_class: []u8,
    venue_id: []u8,

    fn collect(gpa: std.mem.Allocator, io: Io, substrate: []const u8) !Facts {
        var uts: linux.utsname = undefined;
        _ = linux.uname(&uts);
        const kernel = try gpa.dupe(u8, std.mem.sliceTo(&uts.release, 0));
        errdefer gpa.free(kernel);
        // procfs reports size 0: a fixed buffer, never a size-driven read.
        const cpuinfo_buf = try gpa.alloc(u8, 1 << 20);
        defer gpa.free(cpuinfo_buf);
        const cpuinfo = Io.Dir.cwd().readFile(io, "/proc/cpuinfo", cpuinfo_buf) catch "";
        const cpu = try baseline.cpuModel(gpa, cpuinfo, @intCast(std.Thread.getCpuCount() catch 1));
        errdefer gpa.free(cpu);
        // The disk class: the bench's substrate word plus the backing filesystem of /build.
        var fs_buf: [4096]u8 = undefined;
        const mounts = Io.Dir.cwd().readFile(io, "/proc/self/mounts", &fs_buf) catch "";
        var fstype: []const u8 = "unknown";
        var it = std.mem.splitScalar(u8, mounts, '\n');
        while (it.next()) |line| {
            var cols = std.mem.tokenizeScalar(u8, line, ' ');
            _ = cols.next();
            const mp = cols.next() orelse continue;
            if (std.mem.eql(u8, mp, "/build")) fstype = cols.next() orelse "unknown";
        }
        const disk_class = try std.fmt.allocPrint(gpa, "{s}-{s}", .{ substrate, fstype });
        errdefer gpa.free(disk_class);
        const id = try baseline.venueId(gpa, kernel, cpu, disk_class);
        return .{ .kernel = kernel, .cpu = cpu, .disk_class = disk_class, .venue_id = id };
    }

    fn deinit(self: *Facts, gpa: std.mem.Allocator) void {
        gpa.free(self.kernel);
        gpa.free(self.cpu);
        gpa.free(self.disk_class);
        gpa.free(self.venue_id);
    }
};

fn venueMissing(io: Io) ?testkit.Why {
    if (std.testing.environ.getPosix(rig_knob) == null) return .{ .class = .other_step, .detail = "the baseline rig runs under `zig build bench-baseline` / `bench-compare` (" ++ rig_knob ++ " unset)" };
    Io.Dir.cwd().access(io, squeezefs_bin, .{}) catch return .{ .class = .other_step, .detail = "the installed binary is absent (the bench-baseline / bench-compare steps install it)" };
    return testkit.venue.root() orelse testkit.venue.devFuse() orelse testkit.venue.capSysAdmin(io);
}

const Session = struct {
    rig: *bench_smoke.Rig,
    header: []u8,

    fn open(gpa: std.mem.Allocator, io: Io, stem: []const u8) !Session {
        const rig = try bench_smoke.Rig.init2(gpa, io, squeezefs_bin, stem, "bench-base-mnt");
        errdefer rig.deinit();
        try rig.sparse("vol.img", meta_len);
        try rig.sparse("d0.img", data_len);
        try rig.sparse("d1.img", data_len);
        {
            const r = try rig.squeezefs(&.{ "format", rig.meta_uri, rig.data_uri });
            defer gpa.free(r.stdout);
            defer gpa.free(r.stderr);
            if (!r.term.success()) std.debug.print("\nformat: {s}\n", .{r.stderr});
            try testing.expect(r.term.success());
        }
        try rig.spawnMount();
        // The text header once (instrument, substrate, posture — the
        // measurement-law fields): the smoke shape prints it and cleans up.
        const h = try rig.squeezefs(&.{ "bench", "--smoke", rig.mp });
        defer gpa.free(h.stdout);
        defer gpa.free(h.stderr);
        return .{ .rig = rig, .header = try gpa.dupe(u8, h.stdout) };
    }

    fn close(self: *Session, gpa: std.mem.Allocator) void {
        self.rig.umount() catch {};
        self.rig.deinit();
        gpa.free(self.header);
    }

    /// One `bench --json` on the baseline shape, folded.
    fn run(self: *Session, gpa: std.mem.Allocator) !baseline.Run {
        var size_buf: [32]u8 = undefined;
        const size = try std.fmt.bufPrint(&size_buf, "{d}", .{file_size});
        var it_buf: [16]u8 = undefined;
        const its = try std.fmt.bufPrint(&it_buf, "{d}", .{iterations});
        var box_buf: [16]u8 = undefined;
        const box = try std.fmt.bufPrint(&box_buf, "{d}", .{time_box_s});
        var t_buf: [16]u8 = undefined;
        const t = try std.fmt.bufPrint(&t_buf, "{d}", .{threads});
        var n_buf: [16]u8 = undefined;
        const n = try std.fmt.bufPrint(&n_buf, "{d}", .{files});
        const r = try self.rig.squeezefs(&.{ "bench", "--json", "-t", t, "-n", n, "-s", size, "-i", its, "--time", box, self.rig.mp });
        defer gpa.free(r.stdout);
        defer gpa.free(r.stderr);
        if (!r.term.success()) std.debug.print("\nbench: {s}\n{s}\n", .{ r.stdout, r.stderr });
        try testing.expect(r.term.success());
        // The CPU hash groups after the mounted passes (the mount is idle then).
        const h = try self.rig.squeezefs(&.{ "bench", "--hash", "--json" });
        defer gpa.free(h.stdout);
        defer gpa.free(h.stderr);
        if (!h.term.success()) std.debug.print("\nbench --hash: {s}\n{s}\n", .{ h.stdout, h.stderr });
        try testing.expect(h.term.success());
        const rows = try std.mem.concat(gpa, u8, &.{ r.stdout, h.stdout });
        defer gpa.free(rows);
        const folded = try baseline.foldRun(gpa, rows);
        if (!folded.rows_long_enough) std.debug.print("bench baseline: the rows:\n{s}\n", .{r.stdout});
        // Every mounted row's must-stay-0 census.
        var lines = std.mem.tokenizeScalar(u8, r.stdout, '\n');
        while (lines.next()) |line| try testing.expectEqual(@as(?u64, 0), json_scan.number(line, "invariant_tripwires"));
        return folded;
    }

    fn substrate(self: *const Session) []const u8 {
        // `substrate=<word> venue=…` on the header's second line.
        const at = std.mem.indexOf(u8, self.header, "substrate=") orelse return "unknown";
        const rest = self.header[at + "substrate=".len ..];
        return rest[0 .. std.mem.indexOfScalar(u8, rest, ' ') orelse rest.len];
    }

    fn instrument(self: *const Session) baseline.Instrument {
        // `instrument=squeezefs bench <train> <commit> profile=<p>`.
        var out: baseline.Instrument = .{ .train = "unknown", .commit = "unknown", .profile = "unknown" };
        const at = std.mem.indexOf(u8, self.header, "instrument=squeezefs bench ") orelse return out;
        var words = std.mem.tokenizeScalar(u8, self.header[at + "instrument=squeezefs bench ".len ..], ' ');
        out.train = words.next() orelse return out;
        out.commit = words.next() orelse return out;
        const prof = words.next() orelse return out;
        if (std.mem.startsWith(u8, prof, "profile=")) out.profile = prof["profile=".len..];
        return out;
    }
};

fn benchPath(gpa: std.mem.Allocator, name: []const u8) ![]u8 {
    return std.fs.path.join(gpa, &.{ bench_options.bench_dir, name });
}

test "bench baseline: record — 3 runs of the baseline shape through the binary, folded per group with the measurement-law fields, written with the thresholds (max(3 sigma, 5 %)) under the venue id, labelled scoping-venue" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    var s = try Session.open(gpa, io, "bench-baseline");
    defer s.close(gpa);
    var facts = try Facts.collect(gpa, io, s.substrate());
    defer facts.deinit(gpa);
    var folded: [runs]baseline.Run = undefined;
    for (&folded, 0..) |*f, i| {
        f.* = try s.run(gpa);
        if (!f.rows_long_enough) {
            std.debug.print("bench baseline: run {d}: a byte-moving row carried under {d} ops — the shape is too small for this venue; grow file_size or the time box\n", .{ i + 1, baseline.MIN_ROW_OPS });
            return error.ShapeTooSmall;
        }
    }
    const now: u64 = @intCast(@max(0, Io.Timestamp.now(io, .real).toSeconds()));
    const agg = baseline.aggregate(&folded, "scoping-venue", .{ .id = facts.venue_id, .kernel = facts.kernel, .cpu = facts.cpu, .disk_class = facts.disk_class }, s.instrument(), s.substrate(), .{ .threads = threads, .files = files, .size = file_size, .iterations = iterations, .time_box_s = time_box_s }, now);
    const bj = try baseline.writeJson(gpa, agg.baseline);
    defer gpa.free(bj);
    const tj = try baseline.writeJson(gpa, agg.thresholds);
    defer gpa.free(tj);
    Io.Dir.cwd().createDirPath(io, bench_options.bench_dir) catch {};
    const bname = try baseline.baselineFileName(gpa, facts.venue_id);
    defer gpa.free(bname);
    const tname = try baseline.thresholdsFileName(gpa, facts.venue_id);
    defer gpa.free(tname);
    const bpath = try benchPath(gpa, bname);
    defer gpa.free(bpath);
    const tpath = try benchPath(gpa, tname);
    defer gpa.free(tpath);
    try Io.Dir.cwd().writeFile(io, .{ .sub_path = bpath, .data = bj });
    try Io.Dir.cwd().writeFile(io, .{ .sub_path = tpath, .data = tj });
    std.debug.print("bench baseline: wrote {s} and {s} ({d} runs; venue {s})\n", .{ bpath, tpath, runs, facts.venue_id });
    for (agg.baseline.groups, 0..) |g, gi| {
        std.debug.print("  {s}: ops/s {d:.1} ± {d:.1} (drop max {d:.1}); p99 {d:.0} ± {d:.0} ns (rise max {d:.0})\n", .{ @tagName(@as(baseline.Group, @fromBackingInt(@intCast(gi)))), g.ops_per_s.mean, g.ops_per_s.sigma, agg.thresholds.groups[gi].ops_per_s_drop_max, g.p99_ns.mean, g.p99_ns.sigma, agg.thresholds.groups[gi].p99_ns_rise_max });
    }
}

test "bench compare: judge — one run of the baseline shape against the committed baseline of THIS venue under its thresholds; a regression past a threshold fails; no committed baseline for this venue skips" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    var s = try Session.open(gpa, io, "bench-compare");
    defer s.close(gpa);
    var facts = try Facts.collect(gpa, io, s.substrate());
    defer facts.deinit(gpa);
    const bname = try baseline.baselineFileName(gpa, facts.venue_id);
    defer gpa.free(bname);
    const tname = try baseline.thresholdsFileName(gpa, facts.venue_id);
    defer gpa.free(tname);
    const bpath = try benchPath(gpa, bname);
    defer gpa.free(bpath);
    const tpath = try benchPath(gpa, tname);
    defer gpa.free(tpath);
    const btext = Io.Dir.cwd().readFileAlloc(io, bpath, gpa, .limited(1 << 20)) catch return testkit.skip(@src(), .no_baseline, bpath);
    defer gpa.free(btext);
    const ttext = Io.Dir.cwd().readFileAlloc(io, tpath, gpa, .limited(1 << 20)) catch return testkit.skip(@src(), .no_baseline, tpath);
    defer gpa.free(ttext);
    var b = try baseline.parseBaseline(gpa, btext);
    defer b.deinit();
    var t = try baseline.parseThresholds(gpa, ttext);
    defer t.deinit();
    try testing.expectEqualStrings(facts.venue_id, b.value.venue.id);
    try testing.expectEqualStrings(facts.venue_id, t.value.venue_id);
    const run = try s.run(gpa);
    const verdicts = baseline.judge(&b.value, &t.value, &run);
    var regressed = false;
    for (verdicts) |v| {
        std.debug.print("  {s}: ops/s {d:.1} vs baseline {d:.1} ({s}); p99 {d:.0} vs {d:.0} ns ({s})\n", .{ @tagName(v.group), v.ops_per_s, v.baseline_ops_per_s, if (v.ops_regressed) "REGRESSED" else "ok", v.p99_ns, v.baseline_p99_ns, if (v.p99_regressed) "REGRESSED" else "ok" });
        regressed = regressed or v.regressed();
    }
    if (regressed) return error.BenchRegression;
}
