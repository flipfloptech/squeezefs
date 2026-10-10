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
        const text = try cwd.readFileAllocOptions(io, path, gpa, .limited(16 << 20), .of(u8), 0);
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
        const text = try cwd.readFileAllocOptions(io, path, gpa, .limited(16 << 20), .of(u8), 0);
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
