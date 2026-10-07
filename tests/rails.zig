//! Static rails (charter §11 "Static rails as tests"): source-scanning
//! convention tests. They link the library for registry facts and scan the
//! source text for literals. Run from the repository root (`zig build rails`
//! sets the cwd).
//!
//! Pattern (`docs/porting/knobs_and_rails.md` §3.4): fixed root lists walked
//! recursively for `.zig`; a file-count sanity floor so a wrong root fails
//! loud; exemption lists are explicit constants, never heuristics; the failure
//! message lists every violation with `path:line`.
//!
//! The scan is `std.zig.Tokenizer`'s: string literals come out as tokens, so
//! a `'"'` char literal or an escaped quote cannot desync it, and `test`
//! blocks are tracked by brace depth so the inverse census counts PRODUCT
//! readers only — a knob read only by a test, or only under `tests/`, is a
//! dead knob (decision 0003 WP-C row 27).

const std = @import("std");
const squeezefs = @import("squeezefs");
const knobs = squeezefs.knobs;

const roots = [_][]const u8{ "src", "tests" };

/// A wrong root (or a cwd that is not the repo) must fail loud, not pass
/// vacuously. Raised as the tree grows.
const min_files = 8;

/// Literals the census ignores: the bare prefixes `isOurs` tests, the
/// deliberate fakes the knob tests use, and this file's scanner fixture.
const census_exempt = [_][]const u8{
    "SQUEEZEFS_",
    "SQZ_",
    "SQUEEZEFS_TEST_",
    "SQUEEZEFS_X",
    "SQUEEZEFS_N",
    "SQUEEZEFS_D",
    "SQUEEZEFS_E",
    "SQUEEZEFS_TYPO_KNOB",
    "SQUEEZEFS_NOT_A_KNOB",
    "SQUEEZEFS_RAIL_FIXTURE_A",
    "SQUEEZEFS_RAIL_FIXTURE_B",
    "SQUEEZEFS_RAIL_FIXTURE_C",
    "SQUEEZEFS_RAIL_FIXTURE_D",
    "SQUEEZEFS_RAIL_FIXTURE_E",
};

const Hit = struct {
    path: []u8,
    line: usize,
    lit: []u8,
    /// Product scope: outside every `test` block and not under `tests/`.
    product: bool,
};

fn isExempt(lit: []const u8) bool {
    for (census_exempt) |e| if (std.mem.eql(u8, e, lit)) return true;
    return false;
}

fn isKnobShaped(lit: []const u8) bool {
    if (!knobs.isOurs(lit)) return false;
    for (lit) |c| switch (c) {
        'A'...'Z', '0'...'9', '_' => {},
        else => return false,
    };
    return true;
}

fn lineOf(text: []const u8, off: usize) usize {
    return std.mem.count(u8, text[0..off], "\n") + 1;
}

/// Tokenize the file; every `string_literal` token is a candidate, tagged
/// with whether it sits inside a `test` block (`test` keyword → the next
/// `{` opens the block; its matching `}` closes it).
fn scanFile(gpa: std.mem.Allocator, path: []const u8, text: [:0]const u8, hits: *std.ArrayList(Hit)) !void {
    const under_tests = std.mem.startsWith(u8, path, "tests/") or std.mem.startsWith(u8, path, "tests\\");
    var tok = std.zig.Tokenizer.init(text);
    var depth: usize = 0;
    // Brace depth at which the current `test` block's body opened; null outside.
    var test_depth: ?usize = null;
    var pending_test = false;
    while (true) {
        const t = tok.next();
        switch (t.tag) {
            .eof => break,
            .keyword_test => pending_test = true,
            .l_brace => {
                depth += 1;
                if (pending_test) {
                    test_depth = depth;
                    pending_test = false;
                }
            },
            .r_brace => {
                if (test_depth) |d| if (d == depth) {
                    test_depth = null;
                };
                depth -|= 1;
            },
            .string_literal => {
                const raw = text[t.loc.start..t.loc.end];
                // Strip the delimiting quotes; a knob-shaped literal has no escapes.
                const lit = raw[1 .. raw.len - 1];
                if (isKnobShaped(lit)) {
                    try hits.append(gpa, .{
                        .path = try gpa.dupe(u8, path),
                        .line = lineOf(text, t.loc.start),
                        .lit = try gpa.dupe(u8, lit),
                        .product = test_depth == null and !under_tests,
                    });
                }
            },
            else => {},
        }
    }
}

fn walk(gpa: std.mem.Allocator, io: std.Io, dir: std.Io.Dir, prefix: []const u8, files: *std.ArrayList([]u8)) !void {
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
            .file => {
                if (std.mem.endsWith(u8, entry.name, ".zig")) try files.append(gpa, sub) else gpa.free(sub);
            },
            else => gpa.free(sub),
        }
    }
}

fn collectHits(gpa: std.mem.Allocator, io: std.Io) !struct { hits: std.ArrayList(Hit), files: usize } {
    var files: std.ArrayList([]u8) = .empty;
    defer {
        for (files.items) |f| gpa.free(f);
        files.deinit(gpa);
    }
    const cwd = std.Io.Dir.cwd();
    for (roots) |root| {
        var d = try cwd.openDir(io, root, .{ .iterate = true });
        defer d.close(io);
        try walk(gpa, io, d, root, &files);
    }
    var hits: std.ArrayList(Hit) = .empty;
    for (files.items) |path| {
        const text = try cwd.readFileAllocOptions(io, path, gpa, .limited(16 << 20), .of(u8), 0);
        defer gpa.free(text);
        try scanFile(gpa, path, text, &hits);
    }
    return .{ .hits = hits, .files = files.items.len };
}

fn freeHits(gpa: std.mem.Allocator, hits: *std.ArrayList(Hit)) void {
    for (hits.items) |h| {
        gpa.free(h.path);
        gpa.free(h.lit);
    }
    hits.deinit(gpa);
}

test "knob census: every SQUEEZEFS_/SQZ_ literal in the tree is a registered knob" {
    const gpa = std.testing.allocator;
    var r = try collectHits(gpa, std.testing.io);
    defer freeHits(gpa, &r.hits);
    try std.testing.expect(r.files >= min_files);

    var violations: std.Io.Writer.Allocating = .init(gpa);
    defer violations.deinit();
    for (r.hits.items) |h| {
        if (!isExempt(h.lit) and knobs.lookup(h.lit) == null) {
            try violations.writer.print("  {s}:{d}: \"{s}\" is not in the registry (src/core/knobs.zig)\n", .{ h.path, h.line, h.lit });
        }
    }
    if (violations.written().len != 0) {
        std.debug.print("unregistered knob literal(s):\n{s}", .{violations.written()});
        return error.UnregisteredKnobLiteral;
    }
}

test "inverse census: every registered non-retired knob is read by PRODUCT code (no dead knobs; a test-only reader does not count)" {
    const gpa = std.testing.allocator;
    var r = try collectHits(gpa, std.testing.io);
    defer freeHits(gpa, &r.hits);

    var violations: std.Io.Writer.Allocating = .init(gpa);
    defer violations.deinit();
    for (knobs.registry) |k| {
        if (k.kind == .retired) continue;
        var seen = false;
        for (r.hits.items) |h| {
            // The registry row itself is a literal too; a knob must appear in
            // at least one OTHER file, in product scope.
            if (h.product and std.mem.eql(u8, h.lit, k.key) and !std.mem.endsWith(u8, h.path, "core/knobs.zig")) {
                seen = true;
                break;
            }
        }
        if (!seen) try violations.writer.print("  {s}: registered but no product code outside the registry reads it\n", .{k.key});
    }
    if (violations.written().len != 0) {
        std.debug.print("dead knob(s):\n{s}", .{violations.written()});
        return error.DeadKnob;
    }
}

// ---------------------------------------------------------------------------
// The stats inverse rail (charter §10; `docs/porting/stats_cli.md` §1.4):
// every module that DECLARES an instrument (`counters.ShardedCounter`,
// `Gauge`, `Tripwire`, `ShardedLatencyHistogram` as the type of a `pub var`
// or a struct field, outside test blocks) must be reachable from the
// `.stats` tree — a `stats.sources` row (its own family) or a
// `stats.covered` row (a module whose writer composes it). A declared
// instrument nothing exports is dead observability and goes red.
// ---------------------------------------------------------------------------

const stats = squeezefs.stats;

const instrument_types = [_][]const u8{ "ShardedCounter", "Gauge", "Tripwire", "ShardedLatencyHistogram" };

/// The primitives' own module and the aggregator's test fixtures are not
/// product declarations.
const instrument_exempt = [_][]const u8{"src/core/counters.zig"};

/// Does the file declare an instrument in product scope? A declaration is
/// `: counters . <Type>` — the type position of a `pub var` or a field;
/// a pointer (`*counters.Gauge`) or a call (`counters.Gauge.load`) is not.
fn declaresInstrument(text: [:0]const u8) bool {
    var tok = std.zig.Tokenizer.init(text);
    var depth: usize = 0;
    var test_depth: ?usize = null;
    var pending_test = false;
    // The last three significant tokens: `:` `counters` `.` `<Type>`.
    var prev: [3]std.zig.Token.Tag = .{ .invalid, .invalid, .invalid };
    var prev_text: [3][]const u8 = .{ "", "", "" };
    while (true) {
        const t = tok.next();
        if (t.tag == .eof) break;
        switch (t.tag) {
            .keyword_test => pending_test = true,
            .l_brace => {
                depth += 1;
                if (pending_test) {
                    test_depth = depth;
                    pending_test = false;
                }
            },
            .r_brace => {
                if (test_depth) |d| if (d == depth) {
                    test_depth = null;
                };
                depth -|= 1;
            },
            .identifier => {
                const word = text[t.loc.start..t.loc.end];
                if (test_depth == null and prev[0] == .colon and prev[1] == .identifier and std.mem.eql(u8, prev_text[1], "counters") and prev[2] == .period) {
                    for (instrument_types) |ty| if (std.mem.eql(u8, word, ty)) return true;
                }
            },
            else => {},
        }
        prev[0] = prev[1];
        prev[1] = prev[2];
        prev[2] = t.tag;
        prev_text[0] = prev_text[1];
        prev_text[1] = prev_text[2];
        prev_text[2] = text[t.loc.start..t.loc.end];
    }
    return false;
}

fn isExemptInstrumentFile(path: []const u8) bool {
    for (instrument_exempt) |e| if (std.mem.eql(u8, e, path)) return true;
    return false;
}

fn statsPathKnown(path: []const u8) bool {
    for (stats.sources) |s| if (std.mem.eql(u8, s.path, path)) return true;
    for (stats.covered) |c| if (std.mem.eql(u8, c.path, path)) return true;
    return false;
}

test "stats inverse census: every module declaring an instrument is a stats source or covered by one; every source and covered path exists" {
    const gpa = std.testing.allocator;
    const io = std.testing.io;
    var files: std.ArrayList([]u8) = .empty;
    defer {
        for (files.items) |f| gpa.free(f);
        files.deinit(gpa);
    }
    const cwd = std.Io.Dir.cwd();
    {
        var d = try cwd.openDir(io, "src", .{ .iterate = true });
        defer d.close(io);
        try walk(gpa, io, d, "src", &files);
    }
    try std.testing.expect(files.items.len >= min_files);
    var violations: std.Io.Writer.Allocating = .init(gpa);
    defer violations.deinit();
    var declaring: usize = 0;
    for (files.items) |path| {
        if (isExemptInstrumentFile(path)) continue;
        const text = try cwd.readFileAllocOptions(io, path, gpa, .limited(16 << 20), .of(u8), 0);
        defer gpa.free(text);
        if (!declaresInstrument(text)) continue;
        declaring += 1;
        if (!statsPathKnown(path)) try violations.writer.print("  {s}: declares an instrument but no stats family exports it (add a `stats.sources` row, or a `stats.covered` row naming the writer that composes it)\n", .{path});
    }
    // The census must have found the known population (a broken scanner
    // passes vacuously otherwise).
    try std.testing.expect(declaring >= stats.sources.len);
    for (stats.sources) |s| {
        cwd.access(io, s.path, .{}) catch try violations.writer.print("  stats.sources names {s}, which does not exist\n", .{s.path});
    }
    for (stats.covered) |c| {
        cwd.access(io, c.path, .{}) catch try violations.writer.print("  stats.covered names {s}, which does not exist\n", .{c.path});
    }
    if (violations.written().len != 0) {
        std.debug.print("stats inverse census violation(s):\n{s}", .{violations.written()});
        return error.InstrumentNotExported;
    }
}

test "the instrument scanner: a pub var or a field typed counters.<Type> counts; a pointer, a call, a test-block declaration and a plain identifier do not" {
    try std.testing.expect(declaresInstrument("pub var hits: counters.ShardedCounter = .{};"));
    try std.testing.expect(declaresInstrument("const S = struct { depth: counters.Gauge = .{} };"));
    try std.testing.expect(declaresInstrument("pub var tw: counters.Tripwire = .init(\"x\");"));
    try std.testing.expect(!declaresInstrument("fn f(g: *counters.Gauge) void { g.set(1); }"));
    try std.testing.expect(!declaresInstrument("const n = counters.tripwireCount(\"x\");"));
    try std.testing.expect(!declaresInstrument("test \"t\" { var g: counters.Gauge = .{}; _ = &g; }"));
    try std.testing.expect(!declaresInstrument("const counters = @import(\"counters.zig\"); pub var x: u64 = 0;"));
}

test "the scan is a tokenizer's: a '\"' char literal and an escaped quote do not desync it; a test-block literal is not product scope" {
    const gpa = std.testing.allocator;
    const fixture: [:0]const u8 =
        \\const a = '"';
        \\const b = "SQUEEZEFS_RAIL_FIXTURE_A";
        \\const c = "quote \" inside";
        \\const d = "SQUEEZEFS_RAIL_FIXTURE_B";
        \\test "t" {
        \\    const e = "SQUEEZEFS_RAIL_FIXTURE_C";
        \\    if (true) { const f = "SQUEEZEFS_RAIL_FIXTURE_D"; }
        \\}
        \\const g = "SQUEEZEFS_RAIL_FIXTURE_E";
    ;
    var hits: std.ArrayList(Hit) = .empty;
    defer freeHits(gpa, &hits);
    try scanFile(gpa, "src/fixture.zig", fixture, &hits);
    try std.testing.expectEqual(@as(usize, 5), hits.items.len);
    try std.testing.expectEqualStrings("SQUEEZEFS_RAIL_FIXTURE_A", hits.items[0].lit);
    try std.testing.expectEqual(@as(usize, 2), hits.items[0].line);
    try std.testing.expect(hits.items[0].product);
    try std.testing.expectEqualStrings("SQUEEZEFS_RAIL_FIXTURE_B", hits.items[1].lit);
    try std.testing.expect(hits.items[1].product);
    try std.testing.expect(!hits.items[2].product);
    try std.testing.expect(!hits.items[3].product);
    try std.testing.expectEqualStrings("SQUEEZEFS_RAIL_FIXTURE_E", hits.items[4].lit);
    try std.testing.expect(hits.items[4].product);
    // Under tests/ nothing is product scope.
    var hits2: std.ArrayList(Hit) = .empty;
    defer freeHits(gpa, &hits2);
    try scanFile(gpa, "tests/fixture.zig", fixture, &hits2);
    for (hits2.items) |h| try std.testing.expect(!h.product);
}
