//! The ONE volume-URI parser (charter §8; `docs/porting/knobs_and_rails.md`
//! row 8's `sqmeta://` spelling). Two schemes name the two planes:
//! `sqmeta://` a metadata volume, `sqdata://` a data volume. The path after
//! the scheme is ABSOLUTE (`sqmeta:///dev/nvme0n1`, `sqmeta:///build/tmp/
//! vol.img`); a comma separates the members of a set. A relative spelling
//! is refused naming the fix — the reference silently prefixed `/` to it
//! (`parse_block_uri`), which turns a typo into a different device
//! (decision 0002 ledger).
//!
//! Portable; pure; the diagnostic formats on demand.

const std = @import("std");

pub const Scheme = enum {
    meta,
    data,

    pub fn prefix(self: Scheme) []const u8 {
        return switch (self) {
            .meta => "sqmeta://",
            .data => "sqdata://",
        };
    }
};

/// A set of members may name at most this many volumes in one URI: the
/// routing width W = 2^16 is the only structural ceiling (charter §5.4),
/// but one argv word of 64 × PATH_MAX is already past any shell; 64 keeps
/// the fixed parse buffer small and names the limit in the refusal.
pub const MAX_MEMBERS: usize = 64;

pub const Refusal = union(enum) {
    wrong_scheme: struct { want: Scheme, got: []const u8 },
    empty_path: Scheme,
    relative_path: struct { scheme: Scheme, got: []const u8 },
    too_many_members: Scheme,

    pub fn format(self: Refusal, w: *std.Io.Writer) std.Io.Writer.Error!void {
        switch (self) {
            .wrong_scheme => |r| try w.print("'{s}' is not a {s} URI (expected {s}<absolute path>[,<absolute path>…])", .{ r.got, r.want.prefix(), r.want.prefix() }),
            .empty_path => |s| try w.print("{s} names no volume: expected {s}<absolute path>", .{ s.prefix(), s.prefix() }),
            .relative_path => |r| try w.print("{s}{s}: the volume path must be absolute (spell it {s}/{s})", .{ r.scheme.prefix(), r.got, r.scheme.prefix(), r.got }),
            .too_many_members => |s| try w.print("a {s} URI names more than {d} members", .{ s.prefix(), MAX_MEMBERS }),
        }
    }
};

pub const Parsed = struct {
    scheme: Scheme,
    /// Slices into the caller's text; each starts with `/`.
    paths: []const []const u8,

    pub fn first(self: Parsed) []const u8 {
        return self.paths[0];
    }
};

pub const Error = error{Refused};

/// Parse `text` as a `scheme` URI into `buf` (the member paths). Total:
/// every refusal is typed and named in `diag`.
pub fn parse(scheme: Scheme, text: []const u8, buf: *[MAX_MEMBERS][]const u8, diag: *Refusal) Error!Parsed {
    const p = scheme.prefix();
    if (!std.mem.startsWith(u8, text, p)) {
        diag.* = .{ .wrong_scheme = .{ .want = scheme, .got = text } };
        return error.Refused;
    }
    const rest = text[p.len..];
    if (rest.len == 0) {
        diag.* = .{ .empty_path = scheme };
        return error.Refused;
    }
    var n: usize = 0;
    var it = std.mem.splitScalar(u8, rest, ',');
    while (it.next()) |member| {
        if (member.len == 0) {
            diag.* = .{ .empty_path = scheme };
            return error.Refused;
        }
        if (member[0] != '/') {
            diag.* = .{ .relative_path = .{ .scheme = scheme, .got = member } };
            return error.Refused;
        }
        if (n == MAX_MEMBERS) {
            diag.* = .{ .too_many_members = scheme };
            return error.Refused;
        }
        buf[n] = member;
        n += 1;
    }
    return .{ .scheme = scheme, .paths = buf[0..n] };
}

/// Which scheme a word carries, if any (the CLI's argv classifier).
pub fn schemeOf(text: []const u8) ?Scheme {
    inline for (std.enums.values(Scheme)) |s| {
        if (std.mem.startsWith(u8, text, s.prefix())) return s;
    }
    return null;
}

const testing = std.testing;

fn refusalText(buf: []u8, r: Refusal) []const u8 {
    return std.fmt.bufPrint(buf, "{f}", .{r}) catch unreachable;
}

test "sqmeta:// and sqdata:// parse one absolute path or a comma-separated set, slices into the text" {
    var buf: [MAX_MEMBERS][]const u8 = undefined;
    var diag: Refusal = undefined;
    const one = try parse(.meta, "sqmeta:///dev/nvme0n1", &buf, &diag);
    try testing.expectEqual(Scheme.meta, one.scheme);
    try testing.expectEqual(@as(usize, 1), one.paths.len);
    try testing.expectEqualStrings("/dev/nvme0n1", one.first());
    const set = try parse(.data, "sqdata:///dev/nvme1n1,/dev/nvme2n1,/build/tmp/d.img", &buf, &diag);
    try testing.expectEqual(Scheme.data, set.scheme);
    try testing.expectEqual(@as(usize, 3), set.paths.len);
    try testing.expectEqualStrings("/dev/nvme2n1", set.paths[1]);
    try testing.expectEqualStrings("/build/tmp/d.img", set.paths[2]);
}

test "refusals are typed and name the fix: wrong scheme, empty, relative, an empty member, too many members" {
    var buf: [MAX_MEMBERS][]const u8 = undefined;
    var diag: Refusal = undefined;
    var text: [256]u8 = undefined;
    try testing.expectError(error.Refused, parse(.meta, "sqdata:///x", &buf, &diag));
    try testing.expect(diag == .wrong_scheme);
    try testing.expect(std.mem.indexOf(u8, refusalText(&text, diag), "is not a sqmeta:// URI") != null);
    try testing.expectError(error.Refused, parse(.meta, "/dev/nvme0n1", &buf, &diag));
    try testing.expect(diag == .wrong_scheme);
    try testing.expectError(error.Refused, parse(.meta, "sqmeta://", &buf, &diag));
    try testing.expect(diag == .empty_path);
    try testing.expectError(error.Refused, parse(.meta, "sqmeta://dev/nvme0n1", &buf, &diag));
    try testing.expect(diag == .relative_path);
    try testing.expect(std.mem.indexOf(u8, refusalText(&text, diag), "sqmeta:///dev/nvme0n1") != null);
    try testing.expectError(error.Refused, parse(.data, "sqdata:///a,,/b", &buf, &diag));
    try testing.expect(diag == .empty_path);
    var many: std.Io.Writer.Allocating = .init(testing.allocator);
    defer many.deinit();
    try many.writer.writeAll("sqdata://");
    for (0..MAX_MEMBERS + 1) |i| try many.writer.print("{s}/v{d}", .{ if (i == 0) "" else ",", i });
    try testing.expectError(error.Refused, parse(.data, many.written(), &buf, &diag));
    try testing.expect(diag == .too_many_members);
}

test "schemeOf classifies an argv word" {
    try testing.expectEqual(@as(?Scheme, .meta), schemeOf("sqmeta:///x"));
    try testing.expectEqual(@as(?Scheme, .data), schemeOf("sqdata:///x"));
    try testing.expectEqual(@as(?Scheme, null), schemeOf("/mnt/x"));
    try testing.expectEqual(@as(?Scheme, null), schemeOf("--force"));
}
