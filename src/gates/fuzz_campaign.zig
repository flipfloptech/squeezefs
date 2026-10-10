//! THE NIGHTLY FUZZ CAMPAIGN (charter §11 "nightly fuzz"): `zig build fuzz`.
//!
//! The census (`gates/census.zig`, the same scan the rail runs) lists every
//! `test "fuzz: …"` under `src/`; each is one TARGET. For each target the
//! driver runs the build runner's fuzzer on exactly that test
//! (`zig build test -Dtest-filter=<name> --fuzz=<cycles>`) for the time
//! budget `SQUEEZEFS_TEST_FUZZ_SECONDS` (default 60 s; the campaign total
//! is targets × budget), in four launches —
//!
//!   1. `--fuzz=1`: pays the one fuzz-mode rebuild of the filtered test
//!      binary (coverage instrumentation), so the compile never counts
//!      against the budget;
//!   2. `--fuzz=1` again: the build runner's fixed launch overhead on this
//!      venue (cache check, the normal run of the filtered test);
//!   3. `--fuzz=<calibration>`: a cycle count timed, minus the overhead;
//!      grown tenfold until it measures at least half a second, which
//!      fixes the cycles-per-second of THIS target;
//!   4. `--fuzz=<budget × rate>`: the campaign run.
//!
//! The iteration-limited mode is chosen over the time-boxed web-UI mode on
//! purpose: it needs no port and no signal, exits by itself, and every
//! exec is counted exactly (the build runner's `FUZZING REPORT` names the
//! test's qualified name, the pc digest and the cumulative run counts; the
//! driver parses it). Per-target exec counts are honest because every
//! launch holds ONE fuzz test.
//!
//! The corpus persists: the fuzzer keeps a target's inputs under
//! `<cache>/f/<hex(wyhash(qualified name))>/`; the driver relocates that
//! directory onto `<corpus root>/<slug>/` (`SQUEEZEFS_TEST_FUZZ_CORPUS_DIR`,
//! `/build/fuzz-corpus` on the venue) and leaves a symlink in the cache,
//! so a cache wipe loses nothing — on the next campaign the symlink is
//! recreated from the `.name` file the slug directory carries.
//!
//! A crash (the build runner saves the input to `<cache>/f/crash` and
//! prints `input saved to`; its exit status in the iteration-limited mode
//! is 0 regardless, so the text is the verdict) is copied to `<corpus root>/<slug>/crashes/<unix s>.bin`,
//! then MINIMIZED by the replay seam: the kit's `SQUEEZEFS_TEST_FUZZ_REPLAY`
//! feeds a candidate through the same test under the stable toolchain (`zig
//! build test -Dtest-filter=<name>`), and a greedy chunk-removal pass keeps
//! the smallest input that still fails (`<unix s>.min.bin`). The campaign
//! exits 1 with the artefact paths in the report — the red repro is the
//! engineer's next commit (`fuzzkit.slice` of the artefact's bytes into the
//! test's corpus).
//!
//! The report (`docs/gates/fuzz-campaign.md` format) goes to stdout and to
//! `<corpus root>/report.md`: `target | file | execs | corpus | coverage | crashes | s`.

const std = @import("std");
const squeezefs = @import("squeezefs");
const knobs = squeezefs.knobs;
const census = squeezefs.gates.census;
const Io = std.Io;
const Allocator = std.mem.Allocator;

const seconds_knob = "SQUEEZEFS_TEST_FUZZ_SECONDS";
const corpus_dir_knob = "SQUEEZEFS_TEST_FUZZ_CORPUS_DIR";
const replay_knob = "SQUEEZEFS_TEST_FUZZ_REPLAY";

/// The calibration launch's first cycle count (~1 s of a slow decoder: the
/// fuzzer's own batch law targets a second per batch); grown tenfold until
/// the launch measures at least `calibration_min_ns` past the overhead, so a
/// microsecond decoder is not rated off timer noise; capped so a slow one
/// never spends the budget calibrating.
const calibration_cycles_first: u64 = 1000;
const calibration_cycles_cap: u64 = 10_000_000;
const calibration_min_ns: u64 = 500 * std.time.ns_per_ms;
/// The campaign run never asks for fewer cycles than the first calibration
/// (a target slower than 1 cycle/ms still gets a real run) and never more
/// than the fuzzer's u32 corpus bound allows per launch.
const min_campaign_cycles: u64 = calibration_cycles_first;
const max_campaign_cycles: u64 = 1 << 31;
/// The minimizer's launch cap: each probe is one test-binary run (~1 s);
/// 200 probes bound a crash's shrink at a few minutes.
const minimizer_probe_cap: usize = 200;
/// A launch past this is wedged, not slow: the budget plus the worst
/// filtered rebuild the venue has shown (minutes), with headroom.
const launch_timeout_margin_s: u64 = 20 * 60;

/// A day: longer than any nightly window.
const max_budget_s: u64 = 24 * 60 * 60;

const Target = struct {
    name: []u8,
    file: []u8,
    slug: []u8,
    /// Filled from the FUZZING REPORT.
    qualified: ?[]u8 = null,
    execs: u64 = 0,
    coverage_seen: u64 = 0,
    coverage_total: u64 = 0,
    corpus_files: usize = 0,
    crashes: usize = 0,
    wall_s: u64 = 0,
    outcome: Outcome = .ok,

    const Outcome = enum { ok, crash, launch_failed };
};

const Args = struct {
    zig: []const u8,
    cache_dir: []const u8,
    prefix: []const u8,
    repo: []const u8,
    filter: ?[]const u8 = null,
};

fn parseArgs(argv: []const []const u8) !Args {
    var a: Args = .{ .zig = "", .cache_dir = "", .prefix = "", .repo = "" };
    var i: usize = 1;
    while (i < argv.len) : (i += 1) {
        const w = argv[i];
        const next = if (i + 1 < argv.len) argv[i + 1] else return error.MissingValue;
        if (std.mem.eql(u8, w, "--zig")) a.zig = next else if (std.mem.eql(u8, w, "--cache-dir")) a.cache_dir = next else if (std.mem.eql(u8, w, "--prefix")) a.prefix = next else if (std.mem.eql(u8, w, "--repo")) a.repo = next else if (std.mem.eql(u8, w, "--filter")) a.filter = next else return error.UnknownFlag;
        i += 1;
    }
    if (a.zig.len == 0 or a.cache_dir.len == 0 or a.prefix.len == 0 or a.repo.len == 0) return error.MissingValue;
    return a;
}

/// A filesystem-safe name for a target: lower-case alphanumerics, runs of
/// anything else collapsed to `-`, cut at 72 bytes, plus the wyhash of the
/// full name so two long names never collide.
fn slugOf(gpa: Allocator, name: []const u8) ![]u8 {
    var out: std.ArrayList(u8) = .empty;
    defer out.deinit(gpa);
    var dash = false;
    for (name) |c| {
        if (out.items.len >= 72) break;
        if (std.ascii.isAlphanumeric(c)) {
            try out.append(gpa, std.ascii.toLower(c));
            dash = false;
        } else if (!dash and out.items.len != 0) {
            try out.append(gpa, '-');
            dash = true;
        }
    }
    while (out.items.len != 0 and out.items[out.items.len - 1] == '-') _ = out.pop();
    return std.fmt.allocPrint(gpa, "{s}-{x}", .{ out.items, @as(u32, @truncate(std.hash.Wyhash.hash(0, name))) });
}

/// The fuzzer's corpus directory name: `std.fmt.hex` of the wyhash — the
/// integer's bytes in memory order (little-endian), as the fuzzer spells it.
fn corpusDirName(qualified: []const u8) [16]u8 {
    return std.fmt.hex(std.hash.Wyhash.hash(0, qualified));
}

fn walk(gpa: Allocator, io: Io, dir: Io.Dir, prefix: []const u8, files: *std.ArrayList([]u8)) !void {
    var it = dir.iterate();
    while (try it.next(io)) |entry| {
        const sub = try std.fs.path.join(gpa, &.{ prefix, entry.name });
        switch (entry.kind) {
            .directory => {
                defer gpa.free(sub);
                var child = try dir.openDir(io, entry.name, .{ .iterate = true });
                defer child.close(io);
                try walk(gpa, io, child, sub, files);
            },
            .file => if (std.mem.endsWith(u8, entry.name, ".zig")) try files.append(gpa, sub) else gpa.free(sub),
            else => gpa.free(sub),
        }
    }
}

fn lessByName(_: void, a: Target, b: Target) bool {
    return std.mem.lessThan(u8, a.name, b.name);
}

/// The census: every `fuzz:` test under `<repo>/src`, sorted by name.
fn collectTargets(gpa: Allocator, io: Io, repo: []const u8, filter: ?[]const u8) !std.ArrayList(Target) {
    var files: std.ArrayList([]u8) = .empty;
    defer {
        for (files.items) |f| gpa.free(f);
        files.deinit(gpa);
    }
    var repo_dir = try Io.Dir.cwd().openDir(io, repo, .{});
    defer repo_dir.close(io);
    {
        var src = try repo_dir.openDir(io, "src", .{ .iterate = true });
        defer src.close(io);
        try walk(gpa, io, src, "src", &files);
    }
    var targets: std.ArrayList(Target) = .empty;
    errdefer targets.deinit(gpa);
    for (files.items) |path| {
        const text = try repo_dir.readFileAllocOptions(io, path, gpa, .limited(16 << 20), .of(u8), 0);
        defer gpa.free(text);
        var c = try census.scan(gpa, text);
        defer c.deinit(gpa);
        for (c.fuzz_tests.items) |ft| {
            if (filter) |f| if (std.mem.indexOf(u8, ft.name, f) == null) continue;
            try targets.append(gpa, .{ .name = try gpa.dupe(u8, ft.name), .file = try gpa.dupe(u8, path), .slug = try slugOf(gpa, ft.name) });
        }
    }
    std.mem.sort(Target, targets.items, {}, lessByName);
    return targets;
}

const Launch = struct {
    ok: bool,
    wall_ns: u64,
    stderr: []u8,

    fn deinit(self: *Launch, gpa: Allocator) void {
        gpa.free(self.stderr);
    }
};

/// One `zig build test -Dtest-filter=<name> [--fuzz=N]` in the repo.
fn launch(gpa: Allocator, io: Io, a: Args, env: *const std.process.Environ.Map, name: []const u8, fuzz_cycles: ?u64, timeout_s: u64) !Launch {
    const filter_arg = try std.fmt.allocPrint(gpa, "-Dtest-filter={s}", .{name});
    defer gpa.free(filter_arg);
    const fuzz_arg = if (fuzz_cycles) |n| try std.fmt.allocPrint(gpa, "--fuzz={d}", .{n}) else try gpa.dupe(u8, "--summary");
    defer gpa.free(fuzz_arg);
    var argv: std.ArrayList([]const u8) = .empty;
    defer argv.deinit(gpa);
    try argv.appendSlice(gpa, &.{ a.zig, "build", "--cache-dir", a.cache_dir, "--prefix", a.prefix, "test", filter_arg, fuzz_arg });
    if (fuzz_cycles == null) try argv.append(gpa, "none");
    const t0 = std.Io.Timestamp.now(io, .awake);
    const r = try std.process.run(gpa, io, .{ .argv = argv.items, .cwd = .{ .path = a.repo }, .environ_map = env, .timeout = .{ .duration = .{ .raw = .{ .nanoseconds = @intCast(timeout_s * std.time.ns_per_s) }, .clock = .awake } } });
    defer gpa.free(r.stdout);
    const wall: u64 = @intCast(@max(1, t0.untilNow(io, .awake).nanoseconds));
    return .{ .ok = r.term == .exited and r.term.exited == 0, .wall_ns = wall, .stderr = r.stderr };
}

/// `Fuzz test: "<qualified>" (<id>)` / `Runs: a -> b` / `Coverage: … -> s/t`.
fn parseReport(gpa: Allocator, stderr: []const u8, t: *Target) !void {
    if (std.mem.indexOf(u8, stderr, "Fuzz test: \"")) |at| {
        const start = at + "Fuzz test: \"".len;
        const end = std.mem.indexOfPos(u8, stderr, start, "\" (") orelse return;
        if (t.qualified) |q| gpa.free(q);
        t.qualified = try gpa.dupe(u8, stderr[start..end]);
    }
    if (std.mem.indexOf(u8, stderr, "Runs: ")) |at| {
        const line_end = std.mem.indexOfScalarPos(u8, stderr, at, '\n') orelse stderr.len;
        const line = stderr[at + "Runs: ".len .. line_end];
        if (std.mem.indexOf(u8, line, " -> ")) |arrow| {
            const before = std.fmt.parseInt(u64, std.mem.trim(u8, line[0..arrow], " "), 10) catch 0;
            const after = std.fmt.parseInt(u64, std.mem.trim(u8, line[arrow + 4 ..], " "), 10) catch 0;
            t.execs += after -| before;
        }
    }
    if (std.mem.indexOf(u8, stderr, "Coverage: ")) |at| {
        const line_end = std.mem.indexOfScalarPos(u8, stderr, at, '\n') orelse stderr.len;
        const line = stderr[at..line_end];
        if (std.mem.lastIndexOf(u8, line, " -> ")) |arrow| {
            const rest = line[arrow + 4 ..];
            const slash = std.mem.indexOfScalar(u8, rest, '/') orelse return;
            const sp = std.mem.indexOfScalarPos(u8, rest, slash, ' ') orelse rest.len;
            t.coverage_seen = std.fmt.parseInt(u64, rest[0..slash], 10) catch 0;
            t.coverage_total = std.fmt.parseInt(u64, rest[slash + 1 .. sp], 10) catch 0;
        }
    }
}

fn countFiles(io: Io, dir_path: []const u8) usize {
    var d = Io.Dir.cwd().openDir(io, dir_path, .{ .iterate = true }) catch return 0;
    defer d.close(io);
    var n: usize = 0;
    var it = d.iterate();
    while (it.next(io) catch null) |e| {
        // The fuzzer's inputs are hex-numbered files; the locks and our `.name` are not corpus.
        if (e.kind == .file and e.name.len != 0 and std.ascii.isHex(e.name[0]) and !std.mem.eql(u8, e.name, "readlock") and !std.mem.eql(u8, e.name, "synclock")) n += 1;
    }
    return n;
}

/// Before a target's launches: if the slug dir remembers its qualified
/// name, (re)create the cache symlink onto it so the persistent corpus is
/// what the fuzzer loads.
fn linkPersistentCorpus(gpa: Allocator, io: Io, cache_dir: []const u8, corpus_root: []const u8, t: *Target) !void {
    const slug_dir = try std.fs.path.join(gpa, &.{ corpus_root, t.slug });
    defer gpa.free(slug_dir);
    const name_path = try std.fs.path.join(gpa, &.{ slug_dir, ".name" });
    defer gpa.free(name_path);
    const qualified = Io.Dir.cwd().readFileAlloc(io, name_path, gpa, .limited(4096)) catch return;
    defer gpa.free(qualified);
    const q = std.mem.trim(u8, qualified, "\n");
    t.qualified = try gpa.dupe(u8, q);
    try ensureLink(gpa, io, cache_dir, slug_dir, q);
}

fn ensureLink(gpa: Allocator, io: Io, cache_dir: []const u8, slug_dir: []const u8, qualified: []const u8) !void {
    const f_dir = try std.fs.path.join(gpa, &.{ cache_dir, "f" });
    defer gpa.free(f_dir);
    Io.Dir.cwd().createDirPath(io, f_dir) catch {};
    const hash_dir = try std.fs.path.join(gpa, &.{ f_dir, &corpusDirName(qualified) });
    defer gpa.free(hash_dir);
    var link_buf: [std.fs.max_path_bytes]u8 = undefined;
    if (Io.Dir.cwd().readLink(io, hash_dir, &link_buf)) |n| {
        if (std.mem.eql(u8, link_buf[0..n], slug_dir)) return;
        try Io.Dir.cwd().deleteFile(io, hash_dir);
    } else |_| {
        // A real directory from a run before the relocation: move its inputs over.
        if (Io.Dir.cwd().openDir(io, hash_dir, .{ .iterate = true })) |*d| {
            var dd = d.*;
            defer dd.close(io);
            Io.Dir.cwd().createDirPath(io, slug_dir) catch {};
            var dest = try Io.Dir.cwd().openDir(io, slug_dir, .{});
            defer dest.close(io);
            var it = dd.iterate();
            while (try it.next(io)) |e| {
                if (e.kind != .file) continue;
                Io.Dir.rename(dd, e.name, dest, e.name, io) catch {};
            }
            dd.close(io);
            dd = try Io.Dir.cwd().openDir(io, ".", .{});
            try Io.Dir.cwd().deleteTree(io, hash_dir);
        } else |_| {}
    }
    Io.Dir.cwd().createDirPath(io, slug_dir) catch {};
    try Io.Dir.cwd().symLink(io, slug_dir, hash_dir, .{ .is_directory = true });
}

/// After the launches: the corpus lives under the slug dir, the `.name`
/// file remembers the qualified name, the cache holds the symlink.
fn persistCorpus(gpa: Allocator, io: Io, cache_dir: []const u8, corpus_root: []const u8, t: *Target) !void {
    const qualified = t.qualified orelse return;
    const slug_dir = try std.fs.path.join(gpa, &.{ corpus_root, t.slug });
    defer gpa.free(slug_dir);
    try ensureLink(gpa, io, cache_dir, slug_dir, qualified);
    const name_path = try std.fs.path.join(gpa, &.{ slug_dir, ".name" });
    defer gpa.free(name_path);
    try Io.Dir.cwd().writeFile(io, .{ .sub_path = name_path, .data = qualified });
    t.corpus_files = countFiles(io, slug_dir);
}

/// Does the candidate still fail the target under the stable toolchain?
fn stillCrashes(gpa: Allocator, io: Io, a: Args, base_env: *const std.process.Environ.Map, t: *const Target, candidate_path: []const u8) !bool {
    var env = try base_env.clone(gpa);
    defer env.deinit();
    try env.put(replay_knob, candidate_path);
    var l = try launch(gpa, io, a, &env, t.name, null, launch_timeout_margin_s);
    defer l.deinit(gpa);
    return !l.ok;
}

/// Greedy chunk removal: for chunk sizes n/2, n/4, … 1, try deleting each
/// chunk; keep the deletion when the input still crashes. Bounded by the
/// probe cap. Every candidate gets its own file: the replay path is part of
/// the test run's cache manifest (build.zig), so a fresh path is a fresh run.
fn minimize(gpa: Allocator, io: Io, a: Args, env: *const std.process.Environ.Map, t: *const Target, crash: []const u8, out_path: []const u8) ![]u8 {
    var cur = try gpa.dupe(u8, crash);
    errdefer gpa.free(cur);
    var probes: usize = 0;
    var chunk: usize = @max(1, cur.len / 2);
    while (chunk >= 1 and probes < minimizer_probe_cap) {
        var pos: usize = 0;
        var removed_any = false;
        while (pos < cur.len and probes < minimizer_probe_cap) {
            const end = @min(cur.len, pos + chunk);
            const cand = try std.mem.concat(gpa, u8, &.{ cur[0..pos], cur[end..] });
            const probe_path = try std.fmt.allocPrint(gpa, "{s}.probe{d}", .{ out_path, probes });
            defer gpa.free(probe_path);
            try Io.Dir.cwd().writeFile(io, .{ .sub_path = probe_path, .data = cand });
            defer Io.Dir.cwd().deleteFile(io, probe_path) catch {};
            probes += 1;
            if (try stillCrashes(gpa, io, a, env, t, probe_path)) {
                gpa.free(cur);
                cur = cand;
                removed_any = true;
            } else {
                gpa.free(cand);
                pos += chunk;
            }
        }
        if (chunk == 1) break;
        if (!removed_any) chunk /= 2 else chunk = @min(chunk, @max(1, cur.len / 2));
    }
    try Io.Dir.cwd().writeFile(io, .{ .sub_path = out_path, .data = cur });
    return cur;
}

fn saveCrash(gpa: Allocator, io: Io, a: Args, env: *const std.process.Environ.Map, corpus_root: []const u8, t: *Target, report: *Io.Writer) !void {
    const crash_src = try std.fs.path.join(gpa, &.{ a.cache_dir, "f", "crash" });
    defer gpa.free(crash_src);
    const bytes = Io.Dir.cwd().readFileAlloc(io, crash_src, gpa, .limited(64 << 20)) catch {
        try report.print("  {s}: the launch failed but no crash artefact was saved (see the launch's stderr above)\n", .{t.name});
        return;
    };
    defer gpa.free(bytes);
    const crash_dir = try std.fs.path.join(gpa, &.{ corpus_root, t.slug, "crashes" });
    defer gpa.free(crash_dir);
    try Io.Dir.cwd().createDirPath(io, crash_dir);
    const stamp: u64 = @intCast(@max(0, Io.Timestamp.now(io, .real).toSeconds()));
    const raw_path = try std.fmt.allocPrint(gpa, "{s}/{d}.bin", .{ crash_dir, stamp });
    defer gpa.free(raw_path);
    const min_path = try std.fmt.allocPrint(gpa, "{s}/{d}.min.bin", .{ crash_dir, stamp });
    defer gpa.free(min_path);
    try Io.Dir.cwd().writeFile(io, .{ .sub_path = raw_path, .data = bytes });
    t.crashes += 1;
    t.outcome = .crash;
    if (bytes.len == 0) {
        try report.print("  {s}: CRASH, but the build runner saved an EMPTY artefact ({s}) — its input capture raced the crash; re-run the target (`zig build fuzz -Dfuzz-filter=\"{s}\"`) to capture it\n", .{ t.name, raw_path, t.name });
        return;
    }
    // The artefact must reproduce under the stable toolchain before it is worth shrinking.
    if (!try stillCrashes(gpa, io, a, env, t, raw_path)) {
        try report.print("  {s}: crash artefact {s} ({d} B) does NOT reproduce through the replay seam — a fuzzer-mode-only failure (timing, instrumentation); kept unminimized\n", .{ t.name, raw_path, bytes.len });
        return;
    }
    const min = try minimize(gpa, io, a, env, t, bytes, min_path);
    defer gpa.free(min);
    try report.print("  {s}: CRASH — artefact {s} ({d} B), minimized {s} ({d} B); replay: SQUEEZEFS_TEST_FUZZ_REPLAY={s} zig build test -Dtest-filter=\"{s}\"\n", .{ t.name, raw_path, bytes.len, min_path, min.len, min_path, t.name });
}

fn defaultCorpusRoot(gpa: Allocator, io: Io, repo: []const u8) ![]u8 {
    const venue_root = knobs.lookup(corpus_dir_knob).?.default;
    if (Io.Dir.cwd().access(io, std.fs.path.dirname(venue_root).?, .{})) |_| return gpa.dupe(u8, venue_root) else |_| {}
    return std.fs.path.join(gpa, &.{ repo, "fuzz-corpus" });
}

pub fn main(init: std.process.Init) !void {
    const io = init.io;
    const gpa = init.gpa;
    const argv = try init.minimal.args.toSlice(init.arena.allocator());
    var a = parseArgs(argv) catch {
        std.debug.print("usage: fuzz_campaign --zig <zig> --cache-dir <dir> --prefix <dir> --repo <dir> [--filter <substr>]\n", .{});
        std.process.exit(2);
    };
    // Absolute: the cache's corpus symlinks must resolve from inside `f/`,
    // and the report names them.
    const repo_abs = try Io.Dir.cwd().realPathFileAlloc(io, a.repo, gpa);
    defer gpa.free(repo_abs);
    a.repo = repo_abs;
    Io.Dir.cwd().createDirPath(io, a.cache_dir) catch {};
    const cache_abs = try Io.Dir.cwd().realPathFileAlloc(io, a.cache_dir, gpa);
    defer gpa.free(cache_abs);
    a.cache_dir = cache_abs;
    var env = try init.environ_map.clone(gpa);
    defer env.deinit();
    // The replay seam must be OFF for the campaign's own launches.
    _ = env.swapRemove(replay_knob);

    // A harness row (never validated by the daemon's gate): parsed here with
    // its own range, the registry's default when absent.
    const budget_s: u64 = switch (knobs.parseIntIn(u64, seconds_knob, env.get(seconds_knob), 1, max_budget_s)) {
        .value => |v| v,
        .absent => comptime std.fmt.parseInt(u64, knobs.row(seconds_knob).default, 10) catch unreachable,
        .invalid => |d| {
            std.debug.print("fuzz campaign: {s}='{s}': expected seconds in 1..{d}\n", .{ seconds_knob, d.raw, max_budget_s });
            std.process.exit(2);
        },
    };
    const corpus_root_raw = if (knobs.present(env.get(corpus_dir_knob))) |p| try gpa.dupe(u8, p) else try defaultCorpusRoot(gpa, io, a.repo);
    defer gpa.free(corpus_root_raw);
    try Io.Dir.cwd().createDirPath(io, corpus_root_raw);
    const corpus_root_z = try Io.Dir.cwd().realPathFileAlloc(io, corpus_root_raw, gpa);
    defer gpa.free(corpus_root_z);
    const corpus_root: []const u8 = corpus_root_z;

    var targets = try collectTargets(gpa, io, a.repo, a.filter);
    defer {
        for (targets.items) |*t| {
            gpa.free(t.name);
            gpa.free(t.file);
            gpa.free(t.slug);
            if (t.qualified) |q| gpa.free(q);
        }
        targets.deinit(gpa);
    }
    if (targets.items.len == 0) {
        std.debug.print("fuzz campaign: no fuzz targets in {s}/src\n", .{a.repo});
        std.process.exit(2);
    }

    var stdout_buf: [8192]u8 = undefined;
    var stdout_w: Io.File.Writer = .init(.stdout(), io, &stdout_buf);
    const out = &stdout_w.interface;
    var notes: Io.Writer.Allocating = .init(gpa);
    defer notes.deinit();

    try out.print("fuzz campaign: {d} targets × {d} s budget = {d} s; corpus root {s}; cache {s}\n", .{ targets.items.len, budget_s, targets.items.len * budget_s, corpus_root, a.cache_dir });
    try out.flush();

    var any_crash = false;
    for (targets.items, 0..) |*t, i| {
        try out.print("[{d}/{d}] {s} ({s})\n", .{ i + 1, targets.items.len, t.name, t.file });
        try out.flush();
        try linkPersistentCorpus(gpa, io, a.cache_dir, corpus_root, t);
        const t0 = Io.Timestamp.now(io, .awake);
        // 1. the rebuild, 2. the overhead, 3. the calibration (grown until it
        // measures), 4. the campaign run.
        var cycles_for_run: u64 = min_campaign_cycles;
        var overhead_ns: u64 = 0;
        var calibration_cycles: u64 = calibration_cycles_first;
        var failed = false;
        var step: usize = 0;
        while (step < 4) {
            const cycles: u64 = switch (step) {
                0, 1 => 1,
                2 => calibration_cycles,
                else => cycles_for_run,
            };
            var l = try launch(gpa, io, a, &env, t.name, cycles, budget_s + launch_timeout_margin_s);
            defer l.deinit(gpa);
            try parseReport(gpa, l.stderr, t);
            if (l.ok and step == 1) overhead_ns = l.wall_ns;
            if (l.ok and step == 2) {
                const fuzz_ns = l.wall_ns -| overhead_ns;
                if (fuzz_ns < calibration_min_ns and calibration_cycles < calibration_cycles_cap) {
                    calibration_cycles *= 10;
                    continue;
                }
                // rate = cycles / (wall − overhead); the run gets budget × rate.
                const rate_per_s = @max(1, calibration_cycles * std.time.ns_per_s / @max(std.time.ns_per_ms, fuzz_ns));
                cycles_for_run = std.math.clamp(budget_s * rate_per_s, min_campaign_cycles, max_campaign_cycles);
            }
            step += 1;
            const crashed = std.mem.indexOf(u8, l.stderr, "input saved to") != null;
            if (!l.ok or crashed) {
                failed = true;
                if (crashed) {
                    try saveCrash(gpa, io, a, &env, corpus_root, t, &notes.writer);
                } else {
                    t.outcome = .launch_failed;
                    try notes.writer.print("  {s}: launch {d} failed without a crash artefact:\n{s}\n", .{ t.name, step + 1, l.stderr });
                }
                break;
            }
        }
        t.wall_s = @intCast(@divTrunc(t0.untilNow(io, .awake).nanoseconds, std.time.ns_per_s));
        try persistCorpus(gpa, io, a.cache_dir, corpus_root, t);
        if (failed) any_crash = true;
        try out.print("    execs {d}, corpus {d}, coverage {d}/{d}, {s}, {d} s\n", .{ t.execs, t.corpus_files, t.coverage_seen, t.coverage_total, @tagName(t.outcome), t.wall_s });
        try out.flush();
    }

    // The report.
    var report: Io.Writer.Allocating = .init(gpa);
    defer report.deinit();
    const w = &report.writer;
    const stamp: u64 = @intCast(@max(0, Io.Timestamp.now(io, .real).toSeconds()));
    try w.print("# Fuzz campaign report\n\nunix time {d}; {d} targets × {d} s budget; corpus root `{s}`.\n\n| target | file | execs | corpus | coverage | crashes | s |\n|---|---|---:|---:|---:|---:|---:|\n", .{ stamp, targets.items.len, budget_s, corpus_root });
    var total_execs: u64 = 0;
    var total_crashes: usize = 0;
    for (targets.items) |t| {
        total_execs += t.execs;
        total_crashes += t.crashes;
        try w.print("| {s} | {s} | {d} | {d} | {d}/{d} | {d}{s} | {d} |\n", .{ t.name, t.file, t.execs, t.corpus_files, t.coverage_seen, t.coverage_total, t.crashes, if (t.outcome == .launch_failed) " (launch failed)" else "", t.wall_s });
    }
    try w.print("\nTotal: {d} execs, {d} crashes.\n", .{ total_execs, total_crashes });
    if (notes.written().len != 0) try w.print("\n## Findings\n\n{s}", .{notes.written()});
    const report_path = try std.fs.path.join(gpa, &.{ corpus_root, "report.md" });
    defer gpa.free(report_path);
    try Io.Dir.cwd().writeFile(io, .{ .sub_path = report_path, .data = report.written() });
    try out.print("\n{s}\nreport written to {s}\n", .{ report.written(), report_path });
    try out.flush();
    if (any_crash) std.process.exit(1);
}
