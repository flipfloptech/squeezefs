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

/// Read a walked file; a file that vanished between the walk and the read
/// (a concurrent agent's `zig fmt` or checkout in the shared tree) has
/// nothing to scan and is skipped, never a rail failure.
fn readWalked(gpa: std.mem.Allocator, io: std.Io, cwd: std.Io.Dir, path: []const u8) !?[:0]u8 {
    return cwd.readFileAllocOptions(io, path, gpa, .limited(16 << 20), .of(u8), 0) catch |err| switch (err) {
        error.FileNotFound => null,
        else => err,
    };
}

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
        const text = try readWalked(gpa, io, cwd, path) orelse continue;
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
        const text = try readWalked(gpa, io, cwd, path) orelse continue;
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

// ---------------------------------------------------------------------------
// The SDK header census (charter §12 "the SDK exports a C ABI"; il_sdk §10.2):
// 0.17's `-femit-h` emits nothing, so `include/squeezefs_sdk.h` is written by
// hand and pinned here — every `export fn sqz_*` in `src/sdk/abi.zig` has a
// prototype in the header with the same arity and the same types (parsed on
// both sides, the Zig type mapped to its C spelling through ONE table), every
// header prototype has its export, and nothing but `sqz_*` is exported.
// ---------------------------------------------------------------------------

const abi_path = "src/sdk/abi.zig";
const header_path = "include/squeezefs_sdk.h";

/// The ONE Zig → C spelling table (canonical C: one space before `*`).
const c_types = [_]struct { zig: []const u8, c: []const u8 }{
    .{ .zig = "?*SqzSession", .c = "sqz_session_t *" },
    .{ .zig = "?*SqzFile", .c = "sqz_file_t *" },
    .{ .zig = "?*SqzBuf", .c = "sqz_buf_t *" },
    .{ .zig = "?[*:0]const u8", .c = "const char *" },
    .{ .zig = "[*:0]const u8", .c = "const char *" },
    .{ .zig = "?*const session.SessionOpts", .c = "const sqz_session_opts_t *" },
    .{ .zig = "?*const session.OpenOpts", .c = "const sqz_open_opts_t *" },
    .{ .zig = "?*session.Stat", .c = "sqz_stat_t *" },
    .{ .zig = "?[*]const session.IoVec", .c = "const sqz_iovec_t *" },
    .{ .zig = "?[*]const session.Sqe", .c = "const sqz_sqe_t *" },
    .{ .zig = "?[*]session.Cqe", .c = "sqz_cqe_t *" },
    .{ .zig = "?*const linux.timespec", .c = "const struct timespec *" },
    .{ .zig = "?*anyopaque", .c = "void *" },
    .{ .zig = "c_int", .c = "int" },
    .{ .zig = "c_uint", .c = "unsigned" },
    .{ .zig = "u32", .c = "uint32_t" },
    .{ .zig = "u64", .c = "uint64_t" },
    .{ .zig = "i64", .c = "int64_t" },
    .{ .zig = "usize", .c = "size_t" },
    .{ .zig = "isize", .c = "ssize_t" },
    .{ .zig = "void", .c = "void" },
};

fn cSpelling(zig_type: []const u8) ?[]const u8 {
    for (c_types) |t| if (std.mem.eql(u8, t.zig, zig_type)) return t.c;
    return null;
}

const Proto = struct {
    name: []const u8,
    ret: []const u8,
    /// Canonical C spellings.
    params: std.ArrayList([]const u8) = .empty,

    fn deinit(self: *Proto, gpa: std.mem.Allocator) void {
        for (self.params.items) |p| gpa.free(p);
        self.params.deinit(gpa);
        gpa.free(self.name);
        gpa.free(self.ret);
    }
};

fn freeProtos(gpa: std.mem.Allocator, list: *std.ArrayList(Proto)) void {
    for (list.items) |*p| p.deinit(gpa);
    list.deinit(gpa);
}

/// Canonical C type text: whitespace collapsed, exactly one space before
/// each `*`, no trailing parameter name.
fn canonicalC(gpa: std.mem.Allocator, raw: []const u8, strip_name: bool) ![]const u8 {
    var s = std.mem.trim(u8, raw, " \t\r\n");
    if (strip_name) {
        // The parameter name is the trailing identifier when the type has
        // anything before it (`void` alone has none).
        var end = s.len;
        while (end > 0 and (std.ascii.isAlphanumeric(s[end - 1]) or s[end - 1] == '_')) end -= 1;
        if (end > 0 and end < s.len) s = std.mem.trim(u8, s[0..end], " \t");
    }
    var out: std.ArrayList(u8) = .empty;
    errdefer out.deinit(gpa);
    var i: usize = 0;
    while (i < s.len) : (i += 1) {
        const c = s[i];
        if (c == ' ' or c == '\t') {
            if (out.items.len > 0 and out.items[out.items.len - 1] != ' ') try out.append(gpa, ' ');
        } else if (c == '*') {
            if (out.items.len > 0 and out.items[out.items.len - 1] != ' ') try out.append(gpa, ' ');
            try out.append(gpa, '*');
        } else try out.append(gpa, c);
    }
    while (out.items.len > 0 and out.items[out.items.len - 1] == ' ') _ = out.pop();
    return out.toOwnedSlice(gpa);
}

/// `export fn sqz_x(a: T, b: U) callconv(.c) R {` → a Proto in C spellings;
/// a Zig type outside the table fails loud (the table is the contract).
fn parseAbiExports(gpa: std.mem.Allocator, text: []const u8, out: *std.ArrayList(Proto), violations: *std.Io.Writer) !void {
    var rest = text;
    var at_start = true;
    while (true) {
        // Line-anchored: the module doc mentions `export fn sqz_*` in prose.
        const at = if (at_start and std.mem.startsWith(u8, rest, "export fn ")) 0 else (std.mem.indexOf(u8, rest, "\nexport fn ") orelse break) + 1;
        at_start = false;
        rest = rest[at + "export fn ".len ..];
        const open = std.mem.indexOfScalar(u8, rest, '(') orelse break;
        const name = rest[0..open];
        if (!std.mem.startsWith(u8, name, "sqz_")) try violations.print("  {s}: export fn {s} is not an sqz_ symbol\n", .{ abi_path, name });
        const close = std.mem.indexOfScalar(u8, rest, ')') orelse break;
        const params_raw = rest[open + 1 .. close];
        const cc = std.mem.indexOf(u8, rest[close..], "callconv(.c) ") orelse break;
        const ret_start = close + cc + "callconv(.c) ".len;
        const ret_end = std.mem.indexOf(u8, rest[ret_start..], " {") orelse break;
        var proto: Proto = .{ .name = try gpa.dupe(u8, name), .ret = undefined };
        const ret_zig = std.mem.trim(u8, rest[ret_start .. ret_start + ret_end], " ");
        proto.ret = try gpa.dupe(u8, cSpelling(ret_zig) orelse blk: {
            try violations.print("  {s}: {s} returns a Zig type the table does not spell: {s}\n", .{ abi_path, name, ret_zig });
            break :blk "?";
        });
        if (std.mem.trim(u8, params_raw, " ").len != 0) {
            var it = std.mem.splitScalar(u8, params_raw, ',');
            while (it.next()) |param| {
                const colon = std.mem.indexOfScalar(u8, param, ':') orelse continue;
                const zig_type = std.mem.trim(u8, param[colon + 1 ..], " ");
                const c = cSpelling(zig_type) orelse blk: {
                    try violations.print("  {s}: {s} takes a Zig type the table does not spell: {s}\n", .{ abi_path, name, zig_type });
                    break :blk "?";
                };
                try proto.params.append(gpa, try gpa.dupe(u8, c));
            }
        }
        try out.append(gpa, proto);
        rest = rest[ret_start + ret_end ..];
    }
}

/// `R sqz_x(T a, U b);` on one line → a Proto in canonical C.
fn parseHeaderProtos(gpa: std.mem.Allocator, text: []const u8, out: *std.ArrayList(Proto)) !void {
    var lines = std.mem.splitScalar(u8, text, '\n');
    while (lines.next()) |line| {
        const t = std.mem.trim(u8, line, " \t\r");
        if (!std.mem.endsWith(u8, t, ");")) continue;
        if (std.mem.startsWith(u8, t, "_Static_assert") or std.mem.startsWith(u8, t, "#")) continue;
        // The name is the identifier right before the first `(` (the return
        // type may itself be an `sqz_*_t`).
        const open = std.mem.indexOfScalar(u8, t, '(') orelse continue;
        var name_at = open;
        while (name_at > 0 and (std.ascii.isAlphanumeric(t[name_at - 1]) or t[name_at - 1] == '_')) name_at -= 1;
        if (!std.mem.startsWith(u8, t[name_at..open], "sqz_")) continue;
        const close = std.mem.lastIndexOfScalar(u8, t, ')') orelse continue;
        var proto: Proto = .{ .name = try gpa.dupe(u8, t[name_at..open]), .ret = undefined };
        proto.ret = try canonicalC(gpa, t[0..name_at], false);
        const params_raw = t[open + 1 .. close];
        if (!std.mem.eql(u8, std.mem.trim(u8, params_raw, " "), "void")) {
            var it = std.mem.splitScalar(u8, params_raw, ',');
            while (it.next()) |param| try proto.params.append(gpa, try canonicalC(gpa, param, true));
        }
        try out.append(gpa, proto);
    }
}

fn sameProto(a: Proto, b: Proto) bool {
    if (!std.mem.eql(u8, a.ret, b.ret) or a.params.items.len != b.params.items.len) return false;
    for (a.params.items, b.params.items) |x, y| if (!std.mem.eql(u8, x, y)) return false;
    return true;
}

test "sdk header census: every export fn sqz_* in abi.zig has a prototype in squeezefs_sdk.h with the same arity and types, every prototype has its export, and nothing but sqz_* is exported" {
    const gpa = std.testing.allocator;
    const io = std.testing.io;
    const cwd = std.Io.Dir.cwd();
    const abi_text = try cwd.readFileAlloc(io, abi_path, gpa, .limited(1 << 20));
    defer gpa.free(abi_text);
    const header_text = try cwd.readFileAlloc(io, header_path, gpa, .limited(1 << 20));
    defer gpa.free(header_text);
    var violations: std.Io.Writer.Allocating = .init(gpa);
    defer violations.deinit();
    var exports: std.ArrayList(Proto) = .empty;
    defer freeProtos(gpa, &exports);
    try parseAbiExports(gpa, abi_text, &exports, &violations.writer);
    var protos: std.ArrayList(Proto) = .empty;
    defer freeProtos(gpa, &protos);
    try parseHeaderProtos(gpa, header_text, &protos);
    // The census must have found the surface (a broken parser passes vacuously).
    try std.testing.expect(exports.items.len >= 20);
    try std.testing.expect(protos.items.len >= 20);
    for (exports.items) |e| {
        var found = false;
        for (protos.items) |p| {
            if (!std.mem.eql(u8, e.name, p.name)) continue;
            found = true;
            if (!sameProto(e, p)) {
                try violations.writer.print("  {s}: {s} differs from the header: zig (", .{ abi_path, e.name });
                for (e.params.items, 0..) |x, i| try violations.writer.print("{s}{s}", .{ if (i == 0) "" else ", ", x });
                try violations.writer.print(") -> {s}; header (", .{e.ret});
                for (p.params.items, 0..) |x, i| try violations.writer.print("{s}{s}", .{ if (i == 0) "" else ", ", x });
                try violations.writer.print(") -> {s}\n", .{p.ret});
            }
        }
        if (!found) try violations.writer.print("  {s}: export fn {s} has no prototype in {s}\n", .{ abi_path, e.name, header_path });
    }
    for (protos.items) |p| {
        var found = false;
        for (exports.items) |e| found = found or std.mem.eql(u8, e.name, p.name);
        if (!found) try violations.writer.print("  {s}: prototype {s} has no export fn in {s}\n", .{ header_path, p.name, abi_path });
    }
    if (violations.written().len != 0) {
        std.debug.print("sdk header census violation(s):\n{s}", .{violations.written()});
        return error.HeaderCensusViolation;
    }
}

test "the header census parsers: a Zig export and a C prototype of the same shape agree; a wrong arity, a wrong type and a non-sqz export are each caught" {
    const gpa = std.testing.allocator;
    var violations: std.Io.Writer.Allocating = .init(gpa);
    defer violations.deinit();
    var exports: std.ArrayList(Proto) = .empty;
    defer freeProtos(gpa, &exports);
    try parseAbiExports(gpa,
        \\export fn sqz_demo(f: ?*SqzFile, iov: ?[*]const session.IoVec, n: c_uint, off: i64) callconv(.c) isize {
        \\export fn sqz_none() callconv(.c) [*:0]const u8 {
        \\export fn bad_name(x: c_int) callconv(.c) c_int {
    , &exports, &violations.writer);
    try std.testing.expectEqual(@as(usize, 3), exports.items.len);
    try std.testing.expect(std.mem.indexOf(u8, violations.written(), "bad_name is not an sqz_ symbol") != null);
    try std.testing.expectEqualStrings("ssize_t", exports.items[0].ret);
    try std.testing.expectEqual(@as(usize, 4), exports.items[0].params.items.len);
    try std.testing.expectEqualStrings("const sqz_iovec_t *", exports.items[0].params.items[1]);
    try std.testing.expectEqual(@as(usize, 0), exports.items[1].params.items.len);
    var protos: std.ArrayList(Proto) = .empty;
    defer freeProtos(gpa, &protos);
    try parseHeaderProtos(gpa,
        \\ssize_t sqz_demo(sqz_file_t *f, const sqz_iovec_t   *iov, unsigned n, int64_t off);
        \\const char *sqz_none(void);
        \\ssize_t sqz_short(sqz_file_t *f);
        \\int sqz_demo_wrong(sqz_file_t *f, const sqz_iovec_t *iov, unsigned n, int64_t off);
        \\_Static_assert(sizeof(sqz_cqe_t) == 16, "cqe");
    , &protos);
    try std.testing.expectEqual(@as(usize, 4), protos.items.len);
    try std.testing.expect(sameProto(exports.items[0], protos.items[0]));
    try std.testing.expect(sameProto(exports.items[1], protos.items[1]));
    try std.testing.expectEqualStrings("const char *", protos.items[1].ret);
    try std.testing.expect(!sameProto(exports.items[0], protos.items[2]));
    try std.testing.expect(!sameProto(exports.items[0], protos.items[3]));
}

// ---------------------------------------------------------------------------
// The skip rail (charter §11: every environment skip routed through one
// testkit): the identifier `SkipZigTest` appears nowhere in the tree but in
// `src/core/testkit.zig`. A bare skip has no class, no ledger row and no
// release-gate verdict.
// ---------------------------------------------------------------------------

const testkit_path = "src/core/testkit.zig";

/// Does the file name `SkipZigTest` as an identifier token (`error.SkipZigTest`,
/// an error-set member)? Prose in a comment or a string does not count.
fn namesSkipZigTest(text: [:0]const u8) ?usize {
    var tok = std.zig.Tokenizer.init(text);
    while (true) {
        const t = tok.next();
        if (t.tag == .eof) return null;
        if (t.tag == .identifier and std.mem.eql(u8, text[t.loc.start..t.loc.end], "SkipZigTest")) return lineOf(text, t.loc.start);
    }
}

test "skip rail: no bare error.SkipZigTest outside the testkit" {
    const gpa = std.testing.allocator;
    const io = std.testing.io;
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
    try std.testing.expect(files.items.len >= min_files);
    var violations: std.Io.Writer.Allocating = .init(gpa);
    defer violations.deinit();
    var kit_seen = false;
    for (files.items) |path| {
        const text = try readWalked(gpa, io, cwd, path) orelse continue;
        defer gpa.free(text);
        const line = namesSkipZigTest(text) orelse continue;
        if (std.mem.eql(u8, path, testkit_path)) {
            kit_seen = true;
            continue;
        }
        try violations.writer.print("  {s}:{d}: a bare SkipZigTest — route it through testkit.skip(@src(), .<class>, \"why\")\n", .{ path, line });
    }
    // The kit itself names the error (a broken scanner passes vacuously otherwise).
    try std.testing.expect(kit_seen);
    if (violations.written().len != 0) {
        std.debug.print("bare skip(s):\n{s}", .{violations.written()});
        return error.BareSkip;
    }
}

test "the skip scanner: an identifier counts, prose in a comment or a string does not" {
    try std.testing.expectEqual(@as(?usize, 2), namesSkipZigTest("const a = 1;\nreturn error.SkipZigTest;\n"));
    try std.testing.expectEqual(@as(?usize, null), namesSkipZigTest("// error.SkipZigTest in prose\nconst s = \"SkipZigTest\";\n"));
}

// ---------------------------------------------------------------------------
// The rig sync rail (decision 0003 §Z): a rig syncs only what it wrote —
// `sync -f <its mount>`, `sync <file>`, fsync. A host-wide `sync` sends
// FUSE_SYNCFS to EVERY FUSE superblock on the host and waits for each: a
// mount another row (or another run on the venue) froze holds it for that
// daemon's whole request timeout, and an older binary's frozen daemon holds
// it forever (the nvmet sigstop-preempt row hung a gate 23 min). Scanned:
// every string literal (a rig's shell script) in `src/` and `tests/`, and
// every shell script under `scripts/`. A lone `"sync"` literal is a name or
// an argv the scanner cannot judge (the frozen-daemon smoke row's host-wide
// sync is that argv: the claim under test). This file's scanner fixture is
// not a rig.
// ---------------------------------------------------------------------------

const script_root = "scripts";
const sync_rail_fixture = "tests/rails.zig";

/// The offset of a host-wide `sync` in shell text: the word `sync` in
/// command position (text start, or after `;` `&` `|` `(` `{` a newline,
/// `then`, `do`, `else`) with no operand (followed by the end, a newline,
/// a separator or a redirection). `sync -f x`, `sync file`, `fsync`,
/// `conv=fsync` and `syncfs` are scoped or other words.
fn globalSyncAt(text: []const u8) ?usize {
    var from: usize = 0;
    while (std.mem.indexOfPos(u8, text, from, "sync")) |at| {
        from = at + 4;
        if (!syncInCommandPosition(text, at)) continue;
        var i = at + 4;
        if (i < text.len) switch (text[i]) {
            ' ', '\t' => {},
            ';', '&', '|', ')', '}', '\n', '>', '<' => return at,
            else => continue,
        };
        while (i < text.len and (text[i] == ' ' or text[i] == '\t')) i += 1;
        if (i == text.len) return at;
        switch (text[i]) {
            ';', '&', '|', ')', '}', '\n', '>', '<' => return at,
            '0'...'9' => if (i + 1 < text.len and text[i + 1] == '>') return at,
            else => {},
        }
    }
    return null;
}

fn syncInCommandPosition(text: []const u8, at: usize) bool {
    var j = at;
    while (j > 0 and (text[j - 1] == ' ' or text[j - 1] == '\t')) j -= 1;
    if (j == 0) return true;
    switch (text[j - 1]) {
        ';', '&', '|', '(', '{', '\n' => return true,
        else => {},
    }
    inline for (.{ "then", "do", "else" }) |kw| {
        if (j >= kw.len and std.mem.eql(u8, text[j - kw.len .. j], kw) and (j == kw.len or !std.ascii.isAlphanumeric(text[j - kw.len - 1]) and text[j - kw.len - 1] != '_')) {
            // A keyword ends where the `sync` begins only across whitespace.
            if (j != at) return true;
        }
    }
    return false;
}

/// A shell line without its comment: from the first `#` that starts a word
/// (`$#`, `${#…}` and `a#b` are not comments).
fn shellCode(line: []const u8) []const u8 {
    var i: usize = 0;
    while (std.mem.indexOfScalarPos(u8, line, i, '#')) |h| {
        if (h == 0 or line[h - 1] == ' ' or line[h - 1] == '\t') return line[0..h];
        i = h + 1;
    }
    return line;
}

fn isShellScript(name: []const u8, text: []const u8) bool {
    if (std.mem.endsWith(u8, name, ".sh")) return true;
    if (!std.mem.startsWith(u8, text, "#!")) return false;
    const first = text[0 .. std.mem.indexOfScalar(u8, text, '\n') orelse text.len];
    return std.mem.endsWith(u8, first, "sh") or std.mem.indexOf(u8, first, "bash") != null or std.mem.indexOf(u8, first, "/sh ") != null;
}

/// Every host-wide `sync` in a Zig file's string literals (one line per
/// multiline-literal token).
fn zigGlobalSyncs(path: []const u8, text: [:0]const u8, out: *std.Io.Writer) !usize {
    var n: usize = 0;
    var tok = std.zig.Tokenizer.init(text);
    while (true) {
        const t = tok.next();
        const lit: []const u8 = switch (t.tag) {
            .eof => break,
            .string_literal => lit: {
                const s = text[t.loc.start + 1 .. t.loc.end - 1];
                if (std.mem.eql(u8, s, "sync")) continue;
                break :lit s;
            },
            .multiline_string_literal_line => text[t.loc.start + 2 .. t.loc.end],
            else => continue,
        };
        if (globalSyncAt(lit) != null) {
            try out.print("  {s}:{d}: a host-wide `sync` in a rig's script\n", .{ path, lineOf(text, t.loc.start) });
            n += 1;
        }
    }
    return n;
}

fn walkAll(gpa: std.mem.Allocator, io: std.Io, dir: std.Io.Dir, prefix: []const u8, files: *std.ArrayList([]u8)) !void {
    var it = dir.iterate();
    while (try it.next(io)) |entry| {
        const sub = try std.fs.path.join(gpa, &.{ prefix, entry.name });
        switch (entry.kind) {
            .directory => {
                defer gpa.free(sub);
                var child = try dir.openDir(io, entry.name, .{ .iterate = true });
                defer child.close(io);
                try walkAll(gpa, io, child, sub, files);
            },
            .file => try files.append(gpa, sub),
            else => gpa.free(sub),
        }
    }
}

test "rig sync rail: no rig script runs a host-wide `sync` — every sync names the rig's own mount or file" {
    const gpa = std.testing.allocator;
    const io = std.testing.io;
    const cwd = std.Io.Dir.cwd();
    var violations: std.Io.Writer.Allocating = .init(gpa);
    defer violations.deinit();
    var zig_files: std.ArrayList([]u8) = .empty;
    defer {
        for (zig_files.items) |f| gpa.free(f);
        zig_files.deinit(gpa);
    }
    for (roots) |root| {
        var d = try cwd.openDir(io, root, .{ .iterate = true });
        defer d.close(io);
        try walk(gpa, io, d, root, &zig_files);
    }
    try std.testing.expect(zig_files.items.len >= min_files);
    var fixture_seen = false;
    for (zig_files.items) |path| {
        if (std.mem.eql(u8, path, sync_rail_fixture)) {
            fixture_seen = true;
            continue;
        }
        const text = try readWalked(gpa, io, cwd, path) orelse continue;
        defer gpa.free(text);
        _ = try zigGlobalSyncs(path, text, &violations.writer);
    }
    try std.testing.expect(fixture_seen);
    var script_files: std.ArrayList([]u8) = .empty;
    defer {
        for (script_files.items) |f| gpa.free(f);
        script_files.deinit(gpa);
    }
    {
        var d = try cwd.openDir(io, script_root, .{ .iterate = true });
        defer d.close(io);
        try walkAll(gpa, io, d, script_root, &script_files);
    }
    var scripts: usize = 0;
    for (script_files.items) |path| {
        const text = try readWalked(gpa, io, cwd, path) orelse continue;
        defer gpa.free(text);
        if (!isShellScript(std.fs.path.basename(path), text)) continue;
        scripts += 1;
        var it = std.mem.splitScalar(u8, text, '\n');
        var line: usize = 0;
        while (it.next()) |l| {
            line += 1;
            if (globalSyncAt(shellCode(l)) != null) try violations.writer.print("  {s}:{d}: a host-wide `sync`\n", .{ path, line });
        }
    }
    // The venue wrapper and the suites' helpers are shell: a scanner that
    // recognizes none passes vacuously.
    try std.testing.expect(scripts >= min_files);
    if (violations.written().len != 0) {
        std.debug.print("host-wide sync(s) in rigs — sync the rig's own mount (`sync -f <mnt>`) or file (`sync <file>`, fsync) instead; a host-wide sync waits on every FUSE mount on the host, a frozen one included (0003 §Z):\n{s}", .{violations.written()});
        return error.HostWideSync;
    }
}

test "the sync scanner: a bare `sync` in command position counts; an operand, an option, fsync, syncfs, prose and a lone name do not" {
    try std.testing.expect(globalSyncAt("sync") != null);
    try std.testing.expect(globalSyncAt("echo a > f && sync && cat f") != null);
    try std.testing.expect(globalSyncAt("rm x; sync; echo ok") != null);
    try std.testing.expect(globalSyncAt("  sync") != null);
    try std.testing.expect(globalSyncAt("sync -f \"$mnt\" 2>/dev/null || sync") != null);
    try std.testing.expect(globalSyncAt("if true; then sync; fi") != null);
    try std.testing.expect(globalSyncAt("sync 2>/dev/null") != null);
    try std.testing.expect(globalSyncAt("( sync )") != null);
    try std.testing.expect(globalSyncAt("echo a > f && sync -f . && cat f") == null);
    try std.testing.expect(globalSyncAt("dd if=x of=y conv=fsync && sync big") == null);
    try std.testing.expect(globalSyncAt("sync d; stat d") == null);
    try std.testing.expect(globalSyncAt("syncfs and fsync and fdatasync") == null);
    try std.testing.expect(globalSyncAt("echo \"sync\"") == null);
    try std.testing.expect(globalSyncAt("resync; async") == null);
    try std.testing.expect(globalSyncAt("thensync;") == null);
    try std.testing.expectEqualStrings("sync -f x ", shellCode("sync -f x # then sync"));
    try std.testing.expectEqualStrings("echo $# ${#a}", shellCode("echo $# ${#a}"));
    try std.testing.expect(isShellScript("venue", "#!/usr/bin/env bash\nset -e\n"));
    try std.testing.expect(isShellScript("x.sh", ""));
    try std.testing.expect(!isShellScript("brk.py", "#!/usr/bin/env python3\n"));
    var out: std.Io.Writer.Allocating = .init(std.testing.allocator);
    defer out.deinit();
    try std.testing.expectEqual(@as(usize, 2), try zigGlobalSyncs("f.zig",
        \\const a = "x && sync && y";
        \\const b = "sync";
        \\const c = "sync -f .";
        \\const d =
        \\    \\rm f
        \\    \\sync
        \\;
    , &out.writer));
}

// ---------------------------------------------------------------------------
// The fuzz census rail (charter §11 "Fuzz every untrusted decoder"): every
// public decoder under the naming law (`src/gates/census.zig`) is NAMED by a
// `test "fuzz: …"` in its own file; every fuzz test calls the ONE entry
// (`fuzzkit.fuzz`); fuzz test names are unique tree-wide (the nightly
// campaign filters by name); nothing calls `std.testing.fuzz` directly but
// the kit. The census IS the list the campaign runs.
// ---------------------------------------------------------------------------

const census = squeezefs.gates.census;
const fuzzkit_path = "src/core/fuzzkit.zig";

/// The known population at the rail's landing (rung 9): a scanner that finds
/// fewer decoders or fuzz tests than this is broken, not a cleaner tree.
const min_decoders = 100;
const min_fuzz_tests = 40;

fn callsStdFuzzDirectly(text: [:0]const u8) ?usize {
    var tok = std.zig.Tokenizer.init(text);
    // `<receiver> . fuzz (` — the receiver is `testing` (or `std.testing`), never the kit.
    var prev: [3]std.zig.Token.Tag = .{ .invalid, .invalid, .invalid };
    var prev_word: [3][]const u8 = .{ "", "", "" };
    while (true) {
        const t = tok.next();
        if (t.tag == .eof) return null;
        if (t.tag == .l_paren and prev[2] == .identifier and std.mem.eql(u8, prev_word[2], "fuzz") and prev[1] == .period and prev[0] == .identifier and std.mem.eql(u8, prev_word[0], "testing")) {
            return lineOf(text, t.loc.start);
        }
        prev[0] = prev[1];
        prev[1] = prev[2];
        prev[2] = t.tag;
        prev_word[0] = prev_word[1];
        prev_word[1] = prev_word[2];
        prev_word[2] = text[t.loc.start..t.loc.end];
    }
}

test "fuzz census: every public decoder has a fuzz test naming it; every fuzz test uses fuzzkit; names are unique; only the kit calls std.testing.fuzz" {
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
    var names: std.StringHashMapUnmanaged([]const u8) = .empty;
    defer {
        var it = names.iterator();
        while (it.next()) |e| {
            gpa.free(e.key_ptr.*);
            gpa.free(e.value_ptr.*);
        }
        names.deinit(gpa);
    }
    var decoders: usize = 0;
    var fuzz_tests: usize = 0;
    for (files.items) |path| {
        const text = try readWalked(gpa, io, cwd, path) orelse continue;
        defer gpa.free(text);
        var c = try census.scan(gpa, text);
        defer c.deinit(gpa);
        decoders += c.decoders.items.len;
        fuzz_tests += c.fuzz_tests.items.len;
        for (c.decoders.items) |d| {
            if (!c.covers(d.id)) try violations.writer.print("  {s}:{d}: decoder `{s}` has no `test \"fuzz: …\"` in this file naming it\n", .{ path, d.line, d.id });
        }
        for (c.fuzz_tests.items) |ft| {
            if (!ft.uses_fuzzkit and !std.mem.eql(u8, path, fuzzkit_path)) try violations.writer.print("  {s}:{d}: `{s}` does not call fuzzkit.fuzz (the ONE entry)\n", .{ path, ft.line, ft.name });
            const gop = try names.getOrPut(gpa, ft.name);
            if (gop.found_existing) {
                try violations.writer.print("  {s}:{d}: fuzz test name `{s}` is also used in {s} — the campaign filters by name; make it unique\n", .{ path, ft.line, ft.name, gop.value_ptr.* });
            } else {
                gop.key_ptr.* = try gpa.dupe(u8, ft.name);
                gop.value_ptr.* = try gpa.dupe(u8, path);
            }
        }
        if (!std.mem.eql(u8, path, fuzzkit_path)) {
            if (callsStdFuzzDirectly(text)) |line| try violations.writer.print("  {s}:{d}: calls std.testing.fuzz directly — use fuzzkit.fuzz\n", .{ path, line });
        }
    }
    if (violations.written().len != 0) {
        std.debug.print("fuzz census violation(s) ({d} decoders, {d} fuzz tests scanned):\n{s}", .{ decoders, fuzz_tests, violations.written() });
        return error.FuzzCensusViolation;
    }
    try std.testing.expect(decoders >= min_decoders);
    try std.testing.expect(fuzz_tests >= min_fuzz_tests);
}

test "the direct-call scanner: `std.testing.fuzz(` and `testing.fuzz(` are caught, `fuzzkit.fuzz(` is not" {
    try std.testing.expect(callsStdFuzzDirectly("try std.testing.fuzz({}, one, .{});") != null);
    try std.testing.expect(callsStdFuzzDirectly("try testing.fuzz({}, one, .{});") != null);
    try std.testing.expect(callsStdFuzzDirectly("try fuzzkit.fuzz({}, one, .{});") == null);
}

// ---------------------------------------------------------------------------
// The literal rail (charter §1.5, §11 "no runtime literal in product code"):
// `src/gates/literals.zig` states the law; every violation it finds in a
// non-harness file under `src/` is either derived, documented on its line,
// or carried in `tests/rails/literal-allowlist.txt` (`path:line | literal |
// reason`) — and a row that matches no violation is STALE and fails, so the
// list only shrinks.
// ---------------------------------------------------------------------------

const literals = squeezefs.gates.literals;
const allowlist_path = "tests/rails/literal-allowlist.txt";

const AllowRow = struct {
    path: []const u8,
    line: usize,
    literal: []const u8,
    reason: []const u8,
    used: bool = false,
};

fn parseAllowlist(gpa: std.mem.Allocator, text: []const u8, out: *std.ArrayList(AllowRow), violations: *std.Io.Writer) !void {
    var lines = std.mem.splitScalar(u8, text, '\n');
    var n: usize = 0;
    while (lines.next()) |raw| {
        n += 1;
        const line = std.mem.trim(u8, raw, " \t\r");
        if (line.len == 0 or line[0] == '#') continue;
        var cols = std.mem.splitSequence(u8, line, " | ");
        const loc = cols.next() orelse continue;
        const lit = cols.next() orelse {
            try violations.print("  {s}:{d}: malformed row (want `path:line | literal | reason`)\n", .{ allowlist_path, n });
            continue;
        };
        const reason = cols.rest();
        if (std.mem.trim(u8, reason, " ").len == 0) {
            try violations.print("  {s}:{d}: a row needs a reason\n", .{ allowlist_path, n });
            continue;
        }
        const colon = std.mem.lastIndexOfScalar(u8, loc, ':') orelse {
            try violations.print("  {s}:{d}: `{s}` is not path:line\n", .{ allowlist_path, n, loc });
            continue;
        };
        const ln = std.fmt.parseInt(usize, loc[colon + 1 ..], 10) catch {
            try violations.print("  {s}:{d}: `{s}` is not path:line\n", .{ allowlist_path, n, loc });
            continue;
        };
        try out.append(gpa, .{ .path = try gpa.dupe(u8, loc[0..colon]), .line = ln, .literal = try gpa.dupe(u8, lit), .reason = reason });
    }
}

test "literal rail: every runtime literal in product code is derived, documented on its line, or allowlisted with a reason; no allowlist row is stale" {
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
    std.mem.sort([]u8, files.items, {}, struct {
        fn lt(_: void, a: []u8, b: []u8) bool {
            return std.mem.lessThan(u8, a, b);
        }
    }.lt);
    var violations: std.Io.Writer.Allocating = .init(gpa);
    defer violations.deinit();
    var allow: std.ArrayList(AllowRow) = .empty;
    defer {
        for (allow.items) |r| {
            gpa.free(r.path);
            gpa.free(r.literal);
        }
        allow.deinit(gpa);
    }
    const allow_text = try cwd.readFileAlloc(io, allowlist_path, gpa, .limited(16 << 20));
    defer gpa.free(allow_text);
    try parseAllowlist(gpa, allow_text, &allow, &violations.writer);
    var found: usize = 0;
    var unlisted: usize = 0;
    var scanned_files: usize = 0;
    for (files.items) |path| {
        if (literals.isHarnessFile(path)) continue;
        scanned_files += 1;
        const text = try readWalked(gpa, io, cwd, path) orelse continue;
        defer gpa.free(text);
        var v = try literals.scan(gpa, text);
        defer v.deinit(gpa);
        for (v.items) |hit| {
            found += 1;
            var listed = false;
            for (allow.items) |*row| {
                if (row.line == hit.line and std.mem.eql(u8, row.path, path) and std.mem.eql(u8, row.literal, hit.literal)) {
                    row.used = true;
                    listed = true;
                }
            }
            if (!listed) {
                unlisted += 1;
                try violations.writer.print("  {s}:{d} | {s} | <derive it, document it on the line, or add this row with a reason>\n", .{ path, hit.line, hit.literal });
            }
        }
    }
    var stale: usize = 0;
    for (allow.items) |row| {
        if (!row.used) {
            stale += 1;
            try violations.writer.print("  {s}: stale row `{s}:{d} | {s}` matches no violation — delete it (the literal moved, was derived, or was documented)\n", .{ allowlist_path, row.path, row.line, row.literal });
        }
    }
    // The census must have found the known population (a broken scanner passes vacuously).
    try std.testing.expect(scanned_files >= min_files);
    if (violations.written().len != 0) {
        std.debug.print("literal rail: {d} runtime literal(s) in {d} product files, {d} unlisted, {d} allowlisted, {d} stale row(s):\n{s}", .{ found, scanned_files, unlisted, allow.items.len, stale, violations.written() });
        return error.LiteralRailViolation;
    }
}

test "the allowlist parser: a row, a comment, a blank, a malformed row and a reasonless row" {
    const gpa = std.testing.allocator;
    var rows: std.ArrayList(AllowRow) = .empty;
    defer {
        for (rows.items) |r| {
            gpa.free(r.path);
            gpa.free(r.literal);
        }
        rows.deinit(gpa);
    }
    var v: std.Io.Writer.Allocating = .init(gpa);
    defer v.deinit();
    try parseAllowlist(gpa,
        \\# a comment
        \\
        \\src/a.zig:12 | 4096 | the page: kernel geometry
        \\src/b.zig:7 | 0o644
        \\nonsense
        \\nope | 5 | a reason
        \\src/c.zig:3 | 9 |
    , &rows, &v.writer);
    try std.testing.expectEqual(@as(usize, 1), rows.items.len);
    try std.testing.expectEqualStrings("src/a.zig", rows.items[0].path);
    try std.testing.expectEqual(@as(usize, 12), rows.items[0].line);
    try std.testing.expectEqualStrings("4096", rows.items[0].literal);
    try std.testing.expect(std.mem.indexOf(u8, v.written(), "malformed") != null);
    try std.testing.expect(std.mem.indexOf(u8, v.written(), "not path:line") != null);
    try std.testing.expect(std.mem.indexOf(u8, v.written(), "needs a reason") != null);
}

// ---------------------------------------------------------------------------
// The bracket ledger rails (charter §11 "Measurement law";
// `docs/evidence/rung-09-brackets.md` §6): `scripts/bracket/brk.py` names
// the `.stats` keys the harness's ledgers read — a second registry of
// counter names — and `src/cli/bench.zig` carries the same term lists.
// Two rails: (1) bench's lists EQUAL brk.py's, so the two ledgers cannot
// drift apart silently; (2) every name in `brk.ALL_COUNTERS` is a key the
// product's `.stats` can emit (an instrument declared in product scope, or
// a key a hand-written family writer prints), so a renamed counter is a
// red gate, not an `absent_keys` entry in a record.
// ---------------------------------------------------------------------------

const bench = squeezefs.cli.bench;
const brk_path = "scripts/bracket/brk.py";

/// The quoted names of the Python list `NAME = [ ... ]` in `text`
/// (brackets never nest in brk.py's term lists; the names carry no escapes).
fn pyListNames(gpa: std.mem.Allocator, text: []const u8, name: []const u8) ![][]const u8 {
    var needle_buf: [128]u8 = undefined;
    const needle = try std.fmt.bufPrint(&needle_buf, "\n{s} = [", .{name});
    const start = std.mem.indexOf(u8, text, needle) orelse return error.PyListNotFound;
    const body_start = start + needle.len;
    const end = std.mem.indexOfScalarPos(u8, text, body_start, ']') orelse return error.PyListUnterminated;
    return pyQuotedNames(gpa, text[body_start..end]);
}

fn pyQuotedNames(gpa: std.mem.Allocator, body: []const u8) ![][]const u8 {
    var out: std.ArrayList([]const u8) = .empty;
    errdefer out.deinit(gpa);
    var i: usize = 0;
    while (std.mem.indexOfScalarPos(u8, body, i, '"')) |open| {
        const close = std.mem.indexOfScalarPos(u8, body, open + 1, '"') orelse return error.PyStringUnterminated;
        try out.append(gpa, body[open + 1 .. close]);
        i = close + 1;
    }
    return out.toOwnedSlice(gpa);
}

/// The list names `ALL_COUNTERS = (A + B + …)` sums, in order.
fn pyAllCountersLists(gpa: std.mem.Allocator, text: []const u8) ![][]const u8 {
    const needle = "\nALL_COUNTERS = (";
    const start = (std.mem.indexOf(u8, text, needle) orelse return error.PyListNotFound) + needle.len;
    const end = std.mem.indexOfScalarPos(u8, text, start, ')') orelse return error.PyListUnterminated;
    var out: std.ArrayList([]const u8) = .empty;
    errdefer out.deinit(gpa);
    var it = std.mem.tokenizeAny(u8, text[start..end], "+ \n\t");
    while (it.next()) |tok| try out.append(gpa, tok);
    return out.toOwnedSlice(gpa);
}

fn expectSameNames(gpa: std.mem.Allocator, label: []const u8, ours: []const []const u8, theirs: []const []const u8, violations: *std.Io.Writer) !void {
    _ = gpa;
    for (ours) |o| {
        var found = false;
        for (theirs) |t| found = found or std.mem.eql(u8, o, t);
        if (!found) try violations.print("  {s}: bench.zig names \"{s}\", brk.py does not\n", .{ label, o });
    }
    for (theirs) |t| {
        var found = false;
        for (ours) |o| found = found or std.mem.eql(u8, o, t);
        if (!found) try violations.print("  {s}: brk.py names \"{s}\", bench.zig does not\n", .{ label, t });
    }
}

test "bracket ledger pin: bench.zig's device-write, request, admission, read-copy, device-read and tick term lists equal brk.py's (the harness's lists were checked against /proc/diskstats; bench follows them)" {
    const gpa = std.testing.allocator;
    const io = std.testing.io;
    const text = try std.Io.Dir.cwd().readFileAlloc(io, brk_path, gpa, .limited(1 << 20));
    defer gpa.free(text);
    var violations: std.Io.Writer.Allocating = .init(gpa);
    defer violations.deinit();
    const Pin = struct { label: []const u8, ours: []const []const u8, min: usize = 2 };
    const pins = [_]Pin{
        .{ .label = "DEVICE_WRITE_ARMS", .ours = &bench.StatsTerms.device_write_keys },
        .{ .label = "DEVICE_WRITE_REQS", .ours = &bench.StatsTerms.device_write_req_keys },
        .{ .label = "ADMISSION_ARMS", .ours = &bench.StatsTerms.admission_keys },
        .{ .label = "READ_COPY_ARMS", .ours = &bench.StatsTerms.read_copy_keys },
        .{ .label = "DEVICE_READ_ARMS", .ours = &bench.StatsTerms.device_read_keys },
        // The META device's two write arms (decision 0003 §L).
        .{ .label = "META_WRITE_ARMS", .ours = &bench.StatsTerms.meta_write_keys },
        // The tick ledgers: the rescues (the transport's commit and CQE
        // lost-wake tripwires among them) and the late arrivals read beside.
        .{ .label = "TICK_RESCUES", .ours = &bench.StatsTerms.tick_rescue_keys },
        .{ .label = "TICK_LATE", .ours = &bench.StatsTerms.tick_late_keys },
    };
    for (pins) |p| {
        const theirs = try pyListNames(gpa, text, p.label);
        defer gpa.free(theirs);
        try std.testing.expect(theirs.len >= p.min);
        try expectSameNames(gpa, p.label, p.ours, theirs, &violations.writer);
    }
    // The rewrite epoch's gauge and the dest-lease gauge are BESIDE in both.
    const beside_w = try pyListNames(gpa, text, "DEVICE_WRITE_BESIDE");
    defer gpa.free(beside_w);
    const beside_r = try pyListNames(gpa, text, "READ_COPY_BESIDE");
    defer gpa.free(beside_r);
    var rewrite_beside = false;
    for (beside_w) |n| rewrite_beside = rewrite_beside or std.mem.eql(u8, n, "rewrite_device_write_bytes");
    var lease_beside = false;
    for (beside_r) |n| lease_beside = lease_beside or std.mem.eql(u8, n, "read_dest_lease_bytes");
    try std.testing.expect(rewrite_beside and lease_beside);
    for (bench.StatsTerms.device_write_keys) |k| try std.testing.expect(!std.mem.eql(u8, k, "rewrite_device_write_bytes"));
    for (bench.StatsTerms.read_copy_keys) |k| try std.testing.expect(!std.mem.eql(u8, k, "read_dest_lease_bytes"));
    // The device-read ledger's TOTAL (the plane's choke point) and the WHERE
    // partition are BESIDE in both: summing them would double every byte.
    const beside_dr = try pyListNames(gpa, text, "DEVICE_READ_BESIDE");
    defer gpa.free(beside_dr);
    var total_beside = false;
    for (beside_dr) |n| total_beside = total_beside or std.mem.eql(u8, n, "data_device_read_bytes");
    try std.testing.expect(total_beside);
    for (bench.StatsTerms.device_read_keys) |k| try std.testing.expect(!std.mem.eql(u8, k, "data_device_read_bytes") and !std.mem.eql(u8, k, "read_fill_dma_bytes") and !std.mem.eql(u8, k, "read_dest_dma_bytes"));
    if (violations.written().len != 0) {
        std.debug.print("bracket ledger pin violation(s):\n{s}", .{violations.written()});
        return error.BracketLedgerDrift;
    }
}

/// The `.stats` keys a product file can emit: every instrument declared in
/// product scope (`name: counters.<Type>`), every tripwire site and every
/// hand-written JSON key (a product-scope string literal equal to the name,
/// or carrying `"name":` as a format string does). Test blocks are skipped.
fn collectStatsKeys(gpa: std.mem.Allocator, text: [:0]const u8, keys: *std.StringHashMapUnmanaged(void)) !void {
    var tok = std.zig.Tokenizer.init(text);
    var depth: usize = 0;
    var test_depth: ?usize = null;
    var pending_test = false;
    var prev: [3]std.zig.Token.Tag = .{ .invalid, .invalid, .invalid };
    var prev_text: [3][]const u8 = .{ "", "", "" };
    // The identifier before `: counters.<Type>` is the instrument's name.
    var ident_before_colon: []const u8 = "";
    // A string inside a builtin call (`@offsetOf(T, "field")`, `@field`,
    // `@hasDecl`) names a Zig member, never a JSON key.
    var paren_depth: usize = 0;
    var builtin_paren: ?usize = null;
    while (true) {
        const t = tok.next();
        if (t.tag == .eof) break;
        const word = text[t.loc.start..t.loc.end];
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
            .l_paren => {
                paren_depth += 1;
                if (prev[2] == .builtin and builtin_paren == null) builtin_paren = paren_depth;
            },
            .r_paren => {
                if (builtin_paren) |b| if (b == paren_depth) {
                    builtin_paren = null;
                };
                paren_depth -|= 1;
            },
            .identifier => if (test_depth == null) {
                if (prev[0] == .colon and prev[1] == .identifier and std.mem.eql(u8, prev_text[1], "counters") and prev[2] == .period) {
                    for (instrument_types) |ty| if (std.mem.eql(u8, word, ty)) try putKey(gpa, keys, ident_before_colon);
                }
            },
            .colon => if (prev[2] == .identifier) {
                ident_before_colon = prev_text[2];
            },
            .string_literal => if (test_depth == null and builtin_paren == null) {
                const lit = word[1 .. word.len - 1];
                if (std.mem.indexOfScalar(u8, lit, '\\') == null) {
                    if (lit.len != 0) try putKey(gpa, keys, lit);
                } else {
                    // A format string: every `\"key\":` inside it.
                    var i: usize = 0;
                    while (std.mem.indexOfPos(u8, lit, i, "\\\"")) |open| {
                        const close = std.mem.indexOfPos(u8, lit, open + 2, "\\\"") orelse break;
                        const key = lit[open + 2 .. close];
                        i = close + 2;
                        if (i < lit.len and lit[i] == ':' and key.len != 0) try putKey(gpa, keys, key);
                    }
                }
            },
            else => {},
        }
        prev[0] = prev[1];
        prev[1] = prev[2];
        prev[2] = t.tag;
        prev_text[0] = prev_text[1];
        prev_text[1] = prev_text[2];
        prev_text[2] = word;
    }
}

/// Owns one copy of each key (a repeat is not re-duplicated).
fn putKey(gpa: std.mem.Allocator, keys: *std.StringHashMapUnmanaged(void), key: []const u8) !void {
    const gop = try keys.getOrPut(gpa, key);
    if (!gop.found_existing) {
        gop.key_ptr.* = try gpa.dupe(u8, key);
        gop.value_ptr.* = {};
    }
}

fn freeKeys(gpa: std.mem.Allocator, keys: *std.StringHashMapUnmanaged(void)) void {
    var it = keys.keyIterator();
    while (it.next()) |k| gpa.free(k.*);
    keys.deinit(gpa);
}

test "stats name census: every name in brk.py's ALL_COUNTERS is a `.stats` key the product emits (an instrument declared in product scope or a key a family writer prints) — a renamed counter is a red gate, not an absent_keys entry" {
    const gpa = std.testing.allocator;
    const io = std.testing.io;
    const cwd = std.Io.Dir.cwd();
    const text = try cwd.readFileAlloc(io, brk_path, gpa, .limited(1 << 20));
    defer gpa.free(text);
    var files: std.ArrayList([]u8) = .empty;
    defer {
        for (files.items) |f| gpa.free(f);
        files.deinit(gpa);
    }
    {
        var d = try cwd.openDir(io, "src", .{ .iterate = true });
        defer d.close(io);
        try walk(gpa, io, d, "src", &files);
    }
    try std.testing.expect(files.items.len >= min_files);
    var keys: std.StringHashMapUnmanaged(void) = .empty;
    defer freeKeys(gpa, &keys);
    for (files.items) |path| {
        const src = try readWalked(gpa, io, cwd, path) orelse continue;
        defer gpa.free(src);
        try collectStatsKeys(gpa, src, &keys);
    }
    // The census must have found the known population.
    try std.testing.expect(keys.contains("fuse_ops") and keys.contains("transport_ring_requests") and keys.contains("ipc_ops_read"));
    const lists = try pyAllCountersLists(gpa, text);
    defer gpa.free(lists);
    try std.testing.expect(lists.len >= 10);
    var violations: std.Io.Writer.Allocating = .init(gpa);
    defer violations.deinit();
    var checked: usize = 0;
    for (lists) |list| {
        const names = try pyListNames(gpa, text, list);
        defer gpa.free(names);
        for (names) |n| {
            checked += 1;
            if (!keys.contains(n)) try violations.writer.print("  {s} ({s}): \"{s}\" is not a `.stats` key any product file emits\n", .{ brk_path, list, n });
        }
    }
    try std.testing.expect(checked >= 100);
    if (violations.written().len != 0) {
        std.debug.print("stats name census: {d} brk.py names checked against {d} product keys:\n{s}", .{ checked, keys.count(), violations.written() });
        return error.StatsNameUnknown;
    }
}

test "the stats key scanner: a declared instrument, a tripwire site, a format-string key and a key list count; a test-block declaration and a doc sentence do not" {
    const gpa = std.testing.allocator;
    var keys: std.StringHashMapUnmanaged(void) = .empty;
    defer freeKeys(gpa, &keys);
    try collectStatsKeys(gpa,
        \\pub var hits: counters.ShardedCounter = .{};
        \\const S = struct { depth: counters.Gauge = .{} };
        \\pub var tw: counters.Tripwire = .init("open_count_stranded");
        \\fn w(x: *Writer) !void { try x.print("{{\"fuse_ops\":{d},\"volume_failed\":{}}}", .{ 1, true }); }
        \\const names = .{ "get_obj", "put_obj" };
        \\comptime { assert(@offsetOf(Page, "page_field") == 0); }
        \\test "t" { var g: counters.Gauge = .{}; _ = &g; _ = "in_a_test"; }
    , &keys);
    try std.testing.expect(keys.contains("hits"));
    try std.testing.expect(keys.contains("depth"));
    try std.testing.expect(keys.contains("open_count_stranded"));
    try std.testing.expect(keys.contains("fuse_ops"));
    try std.testing.expect(keys.contains("volume_failed"));
    try std.testing.expect(keys.contains("get_obj") and keys.contains("put_obj"));
    try std.testing.expect(!keys.contains("g"));
    try std.testing.expect(!keys.contains("in_a_test"));
    try std.testing.expect(!keys.contains("x") and !keys.contains("names"));
    try std.testing.expect(!keys.contains("page_field"));
    // The python list parsers.
    const py = "\nFOO = [\"a\", \"b\",\n       \"c\"]\nBAR = [\"d\"]\nALL_COUNTERS = (FOO + BAR\n                + FOO)\n";
    const foo = try pyListNames(gpa, py, "FOO");
    defer gpa.free(foo);
    try std.testing.expectEqual(@as(usize, 3), foo.len);
    try std.testing.expectEqualStrings("c", foo[2]);
    const lists = try pyAllCountersLists(gpa, py);
    defer gpa.free(lists);
    try std.testing.expectEqual(@as(usize, 3), lists.len);
    try std.testing.expectEqualStrings("BAR", lists[1]);
    try std.testing.expectError(error.PyListNotFound, pyListNames(gpa, py, "NOPE"));
}
