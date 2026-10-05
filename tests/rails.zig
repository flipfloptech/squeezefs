//! Static rails (charter §11 "Static rails as tests"): source-scanning
//! convention tests. They link the library for registry facts and scan the
//! source text for literals. Run from the repository root (`zig build rails`
//! sets the cwd).
//!
//! Pattern (`docs/porting/knobs_and_rails.md` §3.4): fixed root lists walked
//! recursively for `.zig`; a file-count sanity floor so a wrong root fails
//! loud; exemption lists are explicit constants, never heuristics; the failure
//! message lists every violation with `path:line`.

const std = @import("std");
const squeezefs = @import("squeezefs");
const knobs = squeezefs.knobs;

const roots = [_][]const u8{ "src", "tests" };

/// A wrong root (or a cwd that is not the repo) must fail loud, not pass
/// vacuously. Raised as the tree grows.
const min_files = 8;

/// Literals the census ignores: the bare prefixes `isOurs` tests, and the
/// deliberate fakes the knob tests use.
const census_exempt = [_][]const u8{
    "SQUEEZEFS_",
    "SQZ_",
    "SQUEEZEFS_X",
    "SQUEEZEFS_N",
    "SQUEEZEFS_D",
    "SQUEEZEFS_E",
    "SQUEEZEFS_TYPO_KNOB",
    "SQUEEZEFS_NOT_A_KNOB",
};

const Hit = struct { path: []u8, line: usize, lit: []u8 };

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

/// Scan `"`-delimited spans — exactly the reference's scan: pair every quote
/// with the next; the all-uppercase filter rejects the inter-literal text.
fn scanFile(gpa: std.mem.Allocator, path: []const u8, text: []const u8, hits: *std.ArrayList(Hit)) !void {
    var i: usize = 0;
    while (std.mem.indexOfScalarPos(u8, text, i, '"')) |open| {
        const close = std.mem.indexOfScalarPos(u8, text, open + 1, '"') orelse break;
        const lit = text[open + 1 .. close];
        if (isKnobShaped(lit) and !isExempt(lit)) {
            try hits.append(gpa, .{ .path = try gpa.dupe(u8, path), .line = lineOf(text, open), .lit = try gpa.dupe(u8, lit) });
        }
        i = close + 1;
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
        const text = try cwd.readFileAlloc(io, path, gpa, .limited(16 << 20));
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
        if (knobs.lookup(h.lit) == null) {
            try violations.writer.print("  {s}:{d}: \"{s}\" is not in the registry (src/core/knobs.zig)\n", .{ h.path, h.line, h.lit });
        }
    }
    if (violations.written().len != 0) {
        std.debug.print("unregistered knob literal(s):\n{s}", .{violations.written()});
        return error.UnregisteredKnobLiteral;
    }
}

test "inverse census: every registered non-retired knob is read somewhere (no dead knobs)" {
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
            // at least one OTHER file.
            if (std.mem.eql(u8, h.lit, k.key) and !std.mem.endsWith(u8, h.path, "core/knobs.zig")) {
                seen = true;
                break;
            }
        }
        if (!seen) try violations.writer.print("  {s}: registered but no source outside the registry reads it\n", .{k.key});
    }
    if (violations.written().len != 0) {
        std.debug.print("dead knob(s):\n{s}", .{violations.written()});
        return error.DeadKnob;
    }
}
