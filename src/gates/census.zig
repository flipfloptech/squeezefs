//! The decoder / fuzz census (charter §11 "Fuzz every untrusted decoder"):
//! a `std.zig.Tokenizer` scan of one source file that answers two
//! questions the rail (`tests/rails.zig`) and the nightly campaign
//! (`src/gates/fuzz_campaign.zig`) share —
//!
//!   1. which PUBLIC DECODERS the file declares: the naming law is a `pub fn`
//!      whose name starts with `decode`, `parse` or `from` and whose
//!      parameter list takes a CONST BYTE SLICE (`[]const u8`, `[*]const u8`,
//!      `[:0]const u8`, `[]align(N) const u8`, `*const [N]u8`, optional or
//!      not) — bytes from a device, a wire, a file, a sysfs node, an
//!      environment or an argv word; a mutable `[]u8` is an ownership
//!      transfer, not a decode;
//!   2. which of them a `test "fuzz: …"` NAMES: the fuzz closure is every
//!      identifier and every `A.b` pair inside a `fuzz:` test block plus,
//!      to a fixpoint, the bodies of the file's functions those identifiers
//!      reach (a fuzz test typically calls a `decodersTotal(bytes)` helper
//!      that calls each decoder). A free decoder is covered when its name is
//!      in the closure; a method `X.decode` when the pair is, or when both
//!      `X` and `decode` are.
//!
//! The scan is token-exact: strings and comments never desync it, and a
//! `test` block is tracked by brace depth so product scope and test scope
//! are never confused. The fuzz TESTS themselves are the census the campaign
//! runs (one `-Dtest-filter` per name), which is why their names must be
//! unique tree-wide (the rail checks).

const std = @import("std");
const Allocator = std.mem.Allocator;
const Tokenizer = std.zig.Tokenizer;
const Tag = std.zig.Token.Tag;

/// The naming law's prefixes.
pub const decoder_prefixes = [_][]const u8{ "decode", "parse", "from" };

pub const Decoder = struct {
    /// `name` for a free fn, `Container.name` for a method.
    id: []u8,
    line: usize,
};

pub const FuzzTest = struct {
    /// The name without the quotes (`fuzz: …`).
    name: []u8,
    line: usize,
    /// The block calls the ONE entry (`fuzzkit.fuzz`).
    uses_fuzzkit: bool,
};

pub const Census = struct {
    decoders: std.ArrayList(Decoder) = .empty,
    fuzz_tests: std.ArrayList(FuzzTest) = .empty,
    /// Identifiers in the fuzz closure.
    closure: std.StringHashMapUnmanaged(void) = .empty,
    /// `A.b` pairs in the fuzz closure, stored as `A.b`.
    pairs: std.StringHashMapUnmanaged(void) = .empty,

    pub fn deinit(self: *Census, gpa: Allocator) void {
        for (self.decoders.items) |d| gpa.free(d.id);
        self.decoders.deinit(gpa);
        for (self.fuzz_tests.items) |t| gpa.free(t.name);
        self.fuzz_tests.deinit(gpa);
        var it = self.closure.keyIterator();
        while (it.next()) |k| gpa.free(k.*);
        self.closure.deinit(gpa);
        var pit = self.pairs.keyIterator();
        while (pit.next()) |k| gpa.free(k.*);
        self.pairs.deinit(gpa);
    }

    /// The rail's verdict for one decoder.
    pub fn covers(self: *const Census, id: []const u8) bool {
        if (std.mem.indexOfScalar(u8, id, '.')) |dot| {
            if (self.pairs.contains(id)) return true;
            return self.closure.contains(id[0..dot]) and self.closure.contains(id[dot + 1 ..]);
        }
        return self.closure.contains(id);
    }
};

const Fn = struct {
    name: []const u8,
    body_start: usize,
    body_end: usize,
};

const Scope = struct {
    depth: usize,
    /// The container's name (`const X = struct {`), null for an anonymous one.
    name: ?[]const u8,
    is_test: bool,
    is_fuzz_test: bool,
};

fn isDecoderName(name: []const u8) bool {
    for (decoder_prefixes) |p| if (std.mem.startsWith(u8, name, p)) return true;
    return false;
}

/// The parameter text with whitespace removed; a const byte slice shows
/// as `]constu8` (`[]const u8`, `[*]const u8`, `[:0]const u8`), `)constu8`
/// (`[]align(N) const u8`) or `*const[` … `]u8`.
fn takesConstBytes(gpa: Allocator, params: []const u8) !bool {
    var out: std.ArrayList(u8) = .empty;
    defer out.deinit(gpa);
    for (params) |c| if (!std.ascii.isWhitespace(c)) try out.append(gpa, c);
    const p = out.items;
    var from: usize = 0;
    while (std.mem.indexOfPos(u8, p, from, "]constu8")) |at| : (from = at + 1) {
        // The element type of an outer slice (`[]const []const u8`, an
        // argv) is not a byte slice: the `[` of this slice follows `const`.
        const open = std.mem.lastIndexOfScalar(u8, p[0..at], '[') orelse continue;
        if (std.mem.endsWith(u8, p[0..open], "const")) continue;
        return true;
    }
    if (std.mem.indexOf(u8, p, ")constu8") != null) return true;
    if (std.mem.indexOf(u8, p, "*const[")) |at| {
        if (std.mem.indexOfPos(u8, p, at, "]u8") != null) return true;
    }
    return false;
}

fn lineOf(text: []const u8, off: usize) usize {
    return std.mem.count(u8, text[0..off], "\n") + 1;
}

/// Scan one file.
pub fn scan(gpa: Allocator, text: [:0]const u8) !Census {
    var census: Census = .{};
    errdefer census.deinit(gpa);
    var fns: std.ArrayList(Fn) = .empty;
    defer fns.deinit(gpa);
    var scopes: std.ArrayList(Scope) = .empty;
    defer scopes.deinit(gpa);

    var tok = Tokenizer.init(text);
    var depth: usize = 0;
    var prev: [2]std.zig.Token = .{ .{ .tag = .invalid, .loc = .{ .start = 0, .end = 0 } }, .{ .tag = .invalid, .loc = .{ .start = 0, .end = 0 } } };
    // `const X =` seen: the next container keyword's block is X.
    var assigned_name: ?[]const u8 = null;
    var container_pending: ?[]const u8 = null;
    var container_anon_pending = false;
    // `test "name"` seen: the next `{` opens the block.
    var test_pending: ?[]const u8 = null;
    // `fn name(` seen: collect the params, then the body.
    const PendingFn = struct { name: []const u8, is_pub: bool, params_start: usize, params_end: ?usize, paren_depth: usize, line: usize };
    var pending_fn: ?PendingFn = null;
    var pending_fn_body: ?struct { name: []const u8, start_depth: usize, body_start: usize } = null;

    while (true) {
        const t = tok.next();
        if (t.tag == .eof) break;
        const word = text[t.loc.start..t.loc.end];
        switch (t.tag) {
            .keyword_test => {},
            .string_literal => {
                if (prev[1].tag == .keyword_test) test_pending = word[1 .. word.len - 1];
            },
            .keyword_struct, .keyword_union, .keyword_enum, .keyword_opaque => {
                if (assigned_name) |n| container_pending = n else container_anon_pending = true;
            },
            .equal => {
                if (prev[1].tag == .identifier) assigned_name = text[prev[1].loc.start..prev[1].loc.end];
            },
            .keyword_fn => {},
            .identifier => {
                if (prev[1].tag == .keyword_fn) {
                    pending_fn = .{ .name = word, .is_pub = prev[0].tag == .keyword_pub, .params_start = 0, .params_end = null, .paren_depth = 0, .line = lineOf(text, t.loc.start) };
                }
            },
            .l_paren => {
                if (pending_fn) |*pf| {
                    if (pf.params_end == null) {
                        if (pf.paren_depth == 0) pf.params_start = t.loc.end;
                        pf.paren_depth += 1;
                    }
                }
            },
            .r_paren => {
                if (pending_fn) |*pf| {
                    if (pf.params_end == null and pf.paren_depth > 0) {
                        pf.paren_depth -= 1;
                        if (pf.paren_depth == 0) {
                            pf.params_end = t.loc.start;
                            if (pf.is_pub and isDecoderName(pf.name) and try takesConstBytes(gpa, text[pf.params_start..pf.params_end.?])) {
                                // The nearest named container is the method's; a
                                // decoder inside a test block is not product surface.
                                var in_test = false;
                                var container: ?[]const u8 = null;
                                var i = scopes.items.len;
                                while (i > 0) {
                                    i -= 1;
                                    if (scopes.items[i].is_test) {
                                        in_test = true;
                                        break;
                                    }
                                    if (container == null) container = scopes.items[i].name;
                                }
                                if (!in_test) {
                                    const id = if (container) |c| try std.fmt.allocPrint(gpa, "{s}.{s}", .{ c, pf.name }) else try gpa.dupe(u8, pf.name);
                                    try census.decoders.append(gpa, .{ .id = id, .line = pf.line });
                                }
                            }
                        }
                    }
                }
            },
            .l_brace => {
                depth += 1;
                if (test_pending) |name| {
                    try scopes.append(gpa, .{ .depth = depth, .name = null, .is_test = true, .is_fuzz_test = std.mem.startsWith(u8, name, "fuzz:") });
                    if (std.mem.startsWith(u8, name, "fuzz:")) try census.fuzz_tests.append(gpa, .{ .name = try gpa.dupe(u8, name), .line = lineOf(text, prev[1].loc.start), .uses_fuzzkit = false });
                    test_pending = null;
                } else if (container_pending != null or container_anon_pending) {
                    try scopes.append(gpa, .{ .depth = depth, .name = container_pending, .is_test = false, .is_fuzz_test = false });
                    container_pending = null;
                    container_anon_pending = false;
                } else if (pending_fn) |pf| {
                    // `error{…}` in the return type is a set, not the body.
                    if (pf.params_end != null and prev[1].tag != .keyword_error) {
                        pending_fn_body = .{ .name = pf.name, .start_depth = depth, .body_start = t.loc.end };
                        pending_fn = null;
                    }
                }
                assigned_name = null;
            },
            .r_brace => {
                if (scopes.items.len != 0 and scopes.items[scopes.items.len - 1].depth == depth) _ = scopes.pop();
                if (pending_fn_body) |pb| if (pb.start_depth == depth) {
                    try fns.append(gpa, .{ .name = pb.name, .body_start = pb.body_start, .body_end = t.loc.start });
                    pending_fn_body = null;
                };
                depth -|= 1;
            },
            .semicolon => {
                // `fn f() T;` (a prototype) or an expression statement ends a pending fn without a body.
                if (pending_fn) |pf| if (pf.params_end != null) {
                    pending_fn = null;
                };
                assigned_name = null;
            },
            else => {},
        }
        prev[0] = prev[1];
        prev[1] = t;
    }

    // The fuzz closure: identifiers and pairs inside fuzz test blocks, then
    // the bodies of reached functions to a fixpoint.
    try collectIdents(gpa, text, &census, fuzzBlocks(text));
    var changed = true;
    while (changed) {
        changed = false;
        for (fns.items) |f| {
            if (!census.closure.contains(f.name)) continue;
            const before = census.closure.count() + census.pairs.count();
            try collectIdentsRange(gpa, text, &census, f.body_start, f.body_end);
            if (census.closure.count() + census.pairs.count() != before) changed = true;
        }
    }
    for (census.fuzz_tests.items) |*ft| ft.uses_fuzzkit = census.closure.contains("fuzzkit");
    return census;
}

/// A re-tokenization pass yields the `fuzz:` test blocks' byte ranges.
const Range = struct { start: usize, end: usize };

fn fuzzBlocks(text: [:0]const u8) FuzzBlockIterator {
    return .{ .text = text };
}

const FuzzBlockIterator = struct {
    text: [:0]const u8,

    fn each(self: FuzzBlockIterator, gpa: Allocator, out: *std.ArrayList(Range)) !void {
        var tok = Tokenizer.init(self.text);
        var depth: usize = 0;
        var prev_tag: Tag = .invalid;
        var pending = false;
        var open: ?struct { depth: usize, start: usize } = null;
        while (true) {
            const t = tok.next();
            if (t.tag == .eof) break;
            switch (t.tag) {
                .string_literal => if (prev_tag == .keyword_test) {
                    const word = self.text[t.loc.start..t.loc.end];
                    pending = std.mem.startsWith(u8, word[1..], "fuzz:");
                },
                .l_brace => {
                    depth += 1;
                    if (pending) {
                        open = .{ .depth = depth, .start = t.loc.end };
                        pending = false;
                    }
                },
                .r_brace => {
                    if (open) |o| if (o.depth == depth) {
                        try out.append(gpa, .{ .start = o.start, .end = t.loc.start });
                        open = null;
                    };
                    depth -|= 1;
                },
                else => {},
            }
            prev_tag = t.tag;
        }
    }
};

fn collectIdents(gpa: Allocator, text: [:0]const u8, census: *Census, blocks: FuzzBlockIterator) !void {
    var ranges: std.ArrayList(Range) = .empty;
    defer ranges.deinit(gpa);
    try blocks.each(gpa, &ranges);
    for (ranges.items) |r| try collectIdentsRange(gpa, text, census, r.start, r.end);
}

fn collectIdentsRange(gpa: Allocator, text: [:0]const u8, census: *Census, start: usize, end: usize) !void {
    const slice = try gpa.dupeSentinel(u8, text[start..end], 0);
    defer gpa.free(slice);
    var tok = Tokenizer.init(slice);
    var prev: [2]std.zig.Token = .{ .{ .tag = .invalid, .loc = .{ .start = 0, .end = 0 } }, .{ .tag = .invalid, .loc = .{ .start = 0, .end = 0 } } };
    while (true) {
        const t = tok.next();
        if (t.tag == .eof) break;
        if (t.tag == .identifier) {
            const word = slice[t.loc.start..t.loc.end];
            if (!census.closure.contains(word)) try census.closure.put(gpa, try gpa.dupe(u8, word), {});
            if (prev[1].tag == .period and prev[0].tag == .identifier) {
                const pair = try std.fmt.allocPrint(gpa, "{s}.{s}", .{ slice[prev[0].loc.start..prev[0].loc.end], word });
                if (census.pairs.contains(pair)) gpa.free(pair) else try census.pairs.put(gpa, pair, {});
            }
        }
        prev[0] = prev[1];
        prev[1] = t;
    }
}

const testing = std.testing;

test "the naming law: pub decode/parse/from over a const byte slice is a decoder; a mutable slice, a private fn and an unrelated name are not; a method is Container.name" {
    const fixture: [:0]const u8 =
        \\pub fn decodeHeader(bytes: []const u8) !Header {}
        \\pub fn parseCpuList(text: [:0]const u8) ?u32 {}
        \\pub fn fromSysfs(text: []align(8) const u8) Class {}
        \\pub fn decodeFixed(comptime T: type, raw: *const [16]u8) T {}
        \\pub fn fromOwned(gpa: Allocator, data: []u8) !*Buf {}
        \\fn decodePrivate(bytes: []const u8) void {}
        \\pub fn readHeader(bytes: []const u8) void {}
        \\pub fn parseArgs(argv: []const []const u8) void {}
        \\pub const Hello = extern struct {
        \\    pub fn decode(raw: []const u8) !Hello {}
        \\    pub fn encode(self: Hello) void {}
        \\};
        \\pub const Verb = enum(u8) { a = 1, b = 2 };
        \\test "a test-local decoder is not product surface" {
        \\    const T = struct {
        \\        pub fn decodeLocal(b: []const u8) void {}
        \\    };
        \\    _ = T;
        \\}
    ;
    var c = try scan(testing.allocator, fixture);
    defer c.deinit(testing.allocator);
    try testing.expectEqual(@as(usize, 5), c.decoders.items.len);
    try testing.expectEqualStrings("decodeHeader", c.decoders.items[0].id);
    try testing.expectEqualStrings("parseCpuList", c.decoders.items[1].id);
    try testing.expectEqualStrings("fromSysfs", c.decoders.items[2].id);
    try testing.expectEqualStrings("decodeFixed", c.decoders.items[3].id);
    try testing.expectEqualStrings("Hello.decode", c.decoders.items[4].id);
    try testing.expectEqual(@as(usize, 10), c.decoders.items[4].line);
}

test "the fuzz closure: a fuzz test's identifiers, the helpers it reaches (to a fixpoint) and the A.b pairs; covers() by name, by pair, by container+method" {
    const fixture: [:0]const u8 =
        \\pub fn decodeA(bytes: []const u8) void {}
        \\pub fn decodeB(bytes: []const u8) void {}
        \\pub fn decodeC(bytes: []const u8) void {}
        \\pub const Frame = struct {
        \\    pub fn decode(raw: []const u8) void {}
        \\};
        \\pub const Page = struct {
        \\    pub fn decode(raw: []const u8) void {}
        \\};
        \\fn totalOver(bytes: []const u8) void {
        \\    _ = decodeA(bytes);
        \\    deeper(bytes);
        \\}
        \\fn deeper(bytes: []const u8) void {
        \\    _ = decodeB(bytes);
        \\    _ = Frame.decode(bytes);
        \\}
        \\fn unreached(bytes: []const u8) void {
        \\    _ = decodeC(bytes);
        \\}
        \\fn fuzzOne(_: void, smith: *std.testing.Smith) !void {
        \\    var buf: [64]u8 = undefined;
        \\    const n = smith.slice(&buf);
        \\    totalOver(buf[0..n]);
        \\    inline for (.{Page}) |T| _ = T.decode(buf[0..n]);
        \\}
        \\test "fuzz: everything" {
        \\    try fuzzkit.fuzz({}, fuzzOne, .{});
        \\}
        \\test "not a fuzz test" {
        \\    _ = unreached;
        \\}
    ;
    var c = try scan(testing.allocator, fixture);
    defer c.deinit(testing.allocator);
    try testing.expectEqual(@as(usize, 1), c.fuzz_tests.items.len);
    try testing.expectEqualStrings("fuzz: everything", c.fuzz_tests.items[0].name);
    try testing.expect(c.fuzz_tests.items[0].uses_fuzzkit);
    try testing.expectEqual(@as(usize, 5), c.decoders.items.len);
    try testing.expect(c.covers("decodeA"));
    try testing.expect(c.covers("decodeB"));
    try testing.expect(!c.covers("decodeC"));
    try testing.expect(c.covers("Frame.decode"));
    // `Page` and `decode` both appear (the comptime-table style): covered.
    try testing.expect(c.covers("Page.decode"));
    try testing.expect(!c.covers("Nope.decode"));
}

test "a fuzz test that calls std.testing.fuzz directly is seen as not using the kit" {
    const fixture: [:0]const u8 =
        \\fn one(_: void, smith: *std.testing.Smith) !void { _ = smith; }
        \\test "fuzz: direct" {
        \\    try std.testing.fuzz({}, one, .{});
        \\}
    ;
    var c = try scan(testing.allocator, fixture);
    defer c.deinit(testing.allocator);
    try testing.expectEqual(@as(usize, 1), c.fuzz_tests.items.len);
    try testing.expect(!c.fuzz_tests.items[0].uses_fuzzkit);
}
