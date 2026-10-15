//! The "no runtime literal in product code" rail's scanner (charter §1.5
//! "Every constant is DERIVED … a fixed clamp needs a documented
//! physical/format/kernel/measured reason on the line", §11 "Static rails
//! as tests: no runtime literal in product code").
//!
//! THE LAW. A `number_literal` token in PRODUCT SCOPE — a `.zig` file under
//! `src/`, outside every `test` block and ABOVE the tests' preamble (the
//! file-scope line `const testing = std.testing;` opens the test section:
//! the fixtures, rigs and helpers below it serve the tests), in a file that
//! is not a HARNESS (see `isHarnessFile`) — is a violation unless one of
//! these holds:
//!
//!   1. its value is 0, 1 or 2 (arithmetic shape: `+ 1`, `/ 2`, `== 0`,
//!      `[*:0]`);
//!   2. it is an enum field's value (`a = 3,` inside an `enum` container):
//!      a format/wire code the type's doc owns;
//!   3. it is a `switch` prong (`3 => …`, `1...9 =>`): the codec table
//!      that decodes such codes;
//!   4. it sits in an assertion or a comptime block (`assert(…)`,
//!      `@compileError`, `comptime {`): a size/offset/ABI pin, not a knob;
//!   5. it is an INDEX or a WIDTH: inside `[…]` (an array length, a slice
//!      bound, an index — `[8]u8`, `buf[0..4]`, `[off..][0..8]`) or the
//!      shift of a flag bit (`1 << 5`): encoding shape the comptime size
//!      asserts pin, not a knob;
//!   6. it is DOCUMENTED ON THE LINE: the literal's line carries an
//!      end-of-line `//` comment, or a comment heads the contiguous run of
//!      same-indentation lines the literal's statement belongs to (a doc
//!      comment above a group of `const`s documents the group; a blank line
//!      or a line of another indentation ends the group) — the WHY sits
//!      beside the number, as §1.5 asks;
//!   7. its ENCLOSING declaration is documented: the opener line (the
//!      nearest line above with smaller indentation) has a comment above
//!      it or on it — a documented table whose rows restate the formula
//!      need not repeat the WHY per row.
//!
//! Everything else — a bare `4096`, `16 << 30`, `30 * ns_per_s`, `0o644`,
//! `take(4)`, `0xFFFF` — configures behaviour without a reason the reader can check,
//! and is either derived (a `derive.*` function with a tie test), documented
//! on its line, or carried in `tests/rails/literal-allowlist.txt` with a
//! reason — a row that no longer matches a violation fails the rail, so the
//! list can only shrink.
//!
//! A row names its literal by CONTENT, not position: the file, the literal,
//! and `lineHash` of the literal's line (whitespace-trimmed). An edit above
//! the literal leaves its row in force; a changed value or a changed line is
//! a new literal (unlisted) and leaves its row stale. A row covers ONE
//! literal, so a copy of an allowlisted line needs its own row.
//!
//! HARNESS files are test rigs that happen to live under `src/` because they
//! drive the binary (the smoke, the bench smoke, the e2e rigs, the fidelity
//! tier, the kits, the sync test law) and the model-checked MODELS (standalone
//! value types explored exhaustively): their literals are fixture shapes and
//! model bounds, not product behaviour. The name law is explicit below.

const std = @import("std");
const checksum = @import("../core/checksum.zig");
const Allocator = std.mem.Allocator;
const Tokenizer = std.zig.Tokenizer;

pub const Violation = struct {
    line: usize,
    /// The literal's text as written.
    literal: []const u8,
    /// `lineHash` of the literal's line: with the file and the literal, the
    /// identity an allowlist row names.
    line_hash: u64,
};

/// The identity of the line holding `off`: its bytes with the indentation
/// and trailing whitespace trimmed, hashed — a line shift or a re-indent
/// keeps it, any edit to the line's code changes it.
pub fn lineHash(text: []const u8, off: usize) u64 {
    return checksum.hash64(std.mem.trim(u8, text[lineStart(text, off)..lineEnd(text, off)], " \t\r"));
}

/// A file is a harness when its basename carries one of these stems.
pub const harness_stems = [_][]const u8{ "smoke", "e2e", "fidelity", "_model", "testkit", "fuzzkit", "testing" };

pub fn isHarnessFile(path: []const u8) bool {
    const base = std.fs.path.basename(path);
    for (harness_stems) |stem| if (std.mem.indexOf(u8, base, stem) != null) return true;
    return false;
}

fn lineStart(text: []const u8, off: usize) usize {
    return if (std.mem.lastIndexOfScalar(u8, text[0..off], '\n')) |nl| nl + 1 else 0;
}

fn lineEnd(text: []const u8, off: usize) usize {
    return std.mem.indexOfScalarPos(u8, text, off, '\n') orelse text.len;
}

fn isCommentLine(line: []const u8) bool {
    const t = std.mem.trimStart(u8, line, " \t");
    return std.mem.startsWith(u8, t, "//");
}

fn indentOf(line: []const u8) usize {
    var n: usize = 0;
    while (n < line.len and (line[n] == ' ' or line[n] == '\t')) n += 1;
    return n;
}

/// Does the line directly above `line_start` comment?
fn commentAbove(text: []const u8, line_start: usize) bool {
    if (line_start == 0) return false;
    const prev_start = lineStart(text, line_start - 1);
    return isCommentLine(text[prev_start .. line_start - 1]);
}

/// Does a comment head the contiguous run of same-indentation lines that
/// `line_start` belongs to? A blank line or a line of another indentation
/// ends the run.
fn groupDocumented(text: []const u8, line_start: usize) bool {
    const indent = indentOf(text[line_start..lineEnd(text, line_start)]);
    var cur = line_start;
    while (cur > 0) {
        const prev_start = lineStart(text, cur - 1);
        const prev = text[prev_start .. cur - 1];
        if (std.mem.trim(u8, prev, " \t\r").len == 0) return false;
        if (indentOf(prev) != indent) return false;
        if (isCommentLine(prev)) return true;
        cur = prev_start;
    }
    return false;
}

/// An end-of-line comment after `after` on its line (outside strings: the
/// scan happens on the token stream, so a `//` here is text past the last
/// token of interest; a string holding `//` on the same line is rare and
/// errs toward allowing).
fn eolCommentAfter(text: []const u8, after: usize) bool {
    const end = lineEnd(text, after);
    return std.mem.indexOf(u8, text[after..end], "//") != null;
}

/// Value ≤ 2 in any radix, integer only.
fn isShapeValue(lit: []const u8) bool {
    const v = std.fmt.parseInt(u128, lit, 0) catch return false;
    return v <= 2;
}

/// The enclosing declaration's opener: the nearest line above with strictly
/// smaller indentation than the literal's line.
fn openerLineStart(text: []const u8, line_start: usize) ?usize {
    const my_indent = indentOf(text[line_start..lineEnd(text, line_start)]);
    if (my_indent == 0) return null;
    var cur = line_start;
    while (cur > 0) {
        const prev_start = lineStart(text, cur - 1);
        const prev = text[prev_start .. cur - 1];
        if (std.mem.trim(u8, prev, " \t").len != 0 and !isCommentLine(prev) and indentOf(prev) < my_indent) return prev_start;
        cur = prev_start;
    }
    return null;
}

/// Scan one file's product scope for violations (the caller skips harness
/// files and files outside `src/`).
pub fn scan(gpa: Allocator, text: [:0]const u8) !std.ArrayList(Violation) {
    var out: std.ArrayList(Violation) = .empty;
    errdefer out.deinit(gpa);
    const product_end = testPreambleOffset(text) orelse text.len;
    var tok = Tokenizer.init(text);
    var depth: usize = 0;
    var test_depth: ?usize = null;
    var pending_test = false;
    // Container scopes by depth: is the block an enum body? a comptime block?
    const Scope = struct { depth: usize, is_enum: bool, is_comptime: bool, is_switch: bool };
    var scopes: std.ArrayList(Scope) = .empty;
    defer scopes.deinit(gpa);
    // Depths of `.{` struct-literal braces: not blocks, not statement bounds.
    var literal_braces: std.ArrayList(usize) = .empty;
    defer literal_braces.deinit(gpa);
    var enum_pending = false;
    var comptime_pending = false;
    var switch_pending = false;
    var paren_depth_at_switch: ?usize = null;
    var paren_depth: usize = 0;
    var bracket_depth: usize = 0;
    // The statement start offset at the current depth (after `;` `{` `}` `,`).
    var stmt_start: usize = 0;
    // Is the current statement an assertion / compileError call?
    var stmt_has_assert = false;
    var prev: [2]std.zig.Token = .{ .{ .tag = .invalid, .loc = .{ .start = 0, .end = 0 } }, .{ .tag = .invalid, .loc = .{ .start = 0, .end = 0 } } };
    var tokens: std.ArrayList(std.zig.Token) = .empty;
    defer tokens.deinit(gpa);
    while (true) {
        const t = tok.next();
        try tokens.append(gpa, t);
        if (t.tag == .eof) break;
    }
    var i: usize = 0;
    while (i < tokens.items.len) : (i += 1) {
        const t = tokens.items[i];
        if (t.tag == .eof) break;
        const word = text[t.loc.start..t.loc.end];
        switch (t.tag) {
            .keyword_test => pending_test = true,
            .keyword_enum => enum_pending = true,
            .keyword_comptime => comptime_pending = true,
            .keyword_switch => {
                switch_pending = true;
                paren_depth_at_switch = paren_depth;
            },
            .keyword_struct, .keyword_union, .keyword_opaque, .keyword_fn => comptime_pending = false,
            .l_paren => paren_depth += 1,
            .r_paren => paren_depth -|= 1,
            .l_bracket => bracket_depth += 1,
            .r_bracket => bracket_depth -|= 1,
            .identifier, .builtin => {
                if (isAssertName(word)) stmt_has_assert = true;
            },
            .l_brace => {
                depth += 1;
                if (prev[1].tag == .period) {
                    try literal_braces.append(gpa, depth);
                    prev[0] = prev[1];
                    prev[1] = t;
                    continue;
                }
                if (pending_test) {
                    test_depth = depth;
                    pending_test = false;
                }
                // `comptime {` is the block form only when the brace follows the keyword.
                try scopes.append(gpa, .{ .depth = depth, .is_enum = enum_pending, .is_comptime = comptime_pending and prev[1].tag == .keyword_comptime, .is_switch = switch_pending and paren_depth_at_switch != null and paren_depth == paren_depth_at_switch.? });
                enum_pending = false;
                comptime_pending = false;
                switch_pending = false;
                stmt_start = t.loc.end;
                stmt_has_assert = false;
            },
            .r_brace => {
                if (literal_braces.items.len != 0 and literal_braces.items[literal_braces.items.len - 1] == depth) {
                    _ = literal_braces.pop();
                    depth -|= 1;
                    prev[0] = prev[1];
                    prev[1] = t;
                    continue;
                }
                if (test_depth) |d| if (d == depth) {
                    test_depth = null;
                };
                if (scopes.items.len != 0 and scopes.items[scopes.items.len - 1].depth == depth) _ = scopes.pop();
                depth -|= 1;
                stmt_start = t.loc.end;
                stmt_has_assert = false;
            },
            .semicolon, .comma => {
                stmt_start = t.loc.end;
                stmt_has_assert = false;
                enum_pending = false;
                comptime_pending = false;
                switch_pending = false;
            },
            .number_literal => {
                if (test_depth != null or t.loc.start >= product_end) continue;
                if (isShapeValue(word)) continue;
                const scope = if (scopes.items.len != 0) scopes.items[scopes.items.len - 1] else Scope{ .depth = 0, .is_enum = false, .is_comptime = false, .is_switch = false };
                // 2. an enum field value: `name = <lit>` directly in an enum body.
                if (scope.is_enum and prev[1].tag == .equal and prev[0].tag == .identifier) continue;
                // 4. assertion / comptime block (the assertion may follow the literal: `if (x != 120) @compileError`).
                if (scope.is_comptime or stmt_has_assert or stmtHasAssertAhead(text, tokens.items, i)) continue;
                // 3. a switch prong: `<lit> =>`, `<lit>...`, `...<lit>`.
                const next = tokens.items[i + 1].tag;
                if (scope.is_switch and (next == .equal_angle_bracket_right or next == .ellipsis3 or prev[1].tag == .ellipsis3)) continue;
                // 5. an index / width: inside `[…]`, or a flag bit `1 << n`.
                if (bracket_depth != 0) continue;
                if (prev[1].tag == .angle_bracket_angle_bracket_left and prev[0].tag == .number_literal and std.mem.eql(u8, text[prev[0].loc.start..prev[0].loc.end], "1")) continue;
                // 6. documented on the line / heading the statement's group.
                if (eolCommentAfter(text, t.loc.end)) continue;
                const my_line_start = lineStart(text, t.loc.start);
                const first_stmt_line = @min(my_line_start, firstNonBlankLineFrom(text, stmt_start));
                if (groupDocumented(text, my_line_start) or groupDocumented(text, first_stmt_line)) continue;
                // 7. the enclosing declaration's opener is documented.
                if (openerLineStart(text, my_line_start)) |opener| {
                    if (commentAbove(text, opener) or eolCommentAfter(text, lineStart(text, opener))) continue;
                }
                try out.append(gpa, .{ .line = std.mem.count(u8, text[0..t.loc.start], "\n") + 1, .literal = word, .line_hash = lineHash(text, t.loc.start) });
            },
            else => {},
        }
        prev[0] = prev[1];
        prev[1] = t;
    }
    return out;
}

/// The tests' preamble: the file-scope `const testing = std.testing;` line.
fn testPreambleOffset(text: []const u8) ?usize {
    const needle = "const testing = std.testing;";
    var from: usize = 0;
    while (std.mem.indexOfPos(u8, text, from, needle)) |at| : (from = at + 1) {
        if (at == 0 or text[at - 1] == '\n') return at;
    }
    return null;
}

/// `assert`, `assertLayout`, `assertSize`, … and `@compileError`: the
/// statement pins a fact rather than choosing a value.
fn isAssertName(word: []const u8) bool {
    return std.mem.startsWith(u8, word, "assert") or std.mem.eql(u8, word, "@compileError");
}

/// Does the rest of the literal's statement (to the next `;`, `{` or `}`)
/// name an assertion?
fn stmtHasAssertAhead(text: []const u8, tokens: []const std.zig.Token, from: usize) bool {
    var j = from + 1;
    var literal_depth: usize = 0;
    while (j < tokens.len) : (j += 1) {
        const t = tokens[j];
        switch (t.tag) {
            .semicolon, .eof => return false,
            .l_brace => if (tokens[j - 1].tag == .period) {
                literal_depth += 1;
            } else return false,
            .r_brace => if (literal_depth != 0) {
                literal_depth -= 1;
            } else return false,
            .identifier, .builtin => if (isAssertName(text[t.loc.start..t.loc.end])) return true,
            else => {},
        }
    }
    return false;
}

/// The first line holding a token at or after `off` (the statement's first
/// line when the statement starts after a `,` at the end of the previous line).
fn firstNonBlankLineFrom(text: []const u8, off: usize) usize {
    var p = off;
    while (p < text.len and (text[p] == ' ' or text[p] == '\t' or text[p] == '\n' or text[p] == '\r')) p += 1;
    return lineStart(text, @min(p, text.len -| 1));
}

const testing = std.testing;

fn violationLines(gpa: Allocator, text: [:0]const u8) ![]usize {
    var v = try scan(gpa, text);
    defer v.deinit(gpa);
    const lines = try gpa.alloc(usize, v.items.len);
    for (v.items, 0..) |x, i| lines[i] = x.line;
    return lines;
}

test "the law: 0/1/2 pass; a bare literal fails; an EOL comment, a comment heading the group, a documented opener, an enum value, a switch prong, an index, a flag bit, an assert and a test block pass" {
    const fixture: [:0]const u8 =
        \\const a = x + 1;
        \\const b = n / 2;
        \\const c = 4096;
        \\const d = 4096; // the page
        \\/// The ring depth: measured knee.
        \\const e = 128;
        \\pub const Tag = enum(u8) { put = 1, delta = 7, delete = 9 };
        \\fn f(k: u8) u8 {
        \\    return switch (k) {
        \\        3 => 30,
        \\        5...9 => 50,
        \\        else => 0,
        \\    };
        \\}
        \\comptime {
        \\    std.debug.assert(@sizeOf(X) == 120);
        \\}
        \\/// The table.
        \\const table = [_]Row{
        \\    .{ .lo = 4, .hi = 64 },
        \\    .{ .lo = 8, .hi = 512 },
        \\};
        \\const bad = [_]u8{ 5, 6 };
        \\test "t" {
        \\    const z = 999;
        \\}
        \\fn g() void {
        \\    const t = std.time.ns_per_s * 30;
        \\    // the park bound: measured
        \\    const u = std.time.ns_per_s * 30;
        \\    const v = std.time.ns_per_s * 30;
        \\    _ = .{ t, u, v, buf[0..4], @as(u64, 1) << 5 };
        \\}
        \\/// The group's doc.
        \\const g1 = 16;
        \\const g2 = 32;
        \\
        \\const g3 = 64;
        \\const testing = std.testing;
        \\const fixture_len = 4096;
    ;
    const lines = try violationLines(testing.allocator, fixture);
    defer testing.allocator.free(lines);
    // c (line 3), the prong RESULTS 30 and 50 (lines 10, 11), bad's two
    // elements (line 23), t (line 28) — u and v are headed by the comment —
    // and g3 (line 38) past the blank line; the fixture past the tests'
    // preamble is test scope.
    try testing.expectEqualSlices(usize, &.{ 3, 10, 11, 23, 23, 28, 38 }, lines);
}

test "a line's identity: trimmed of indentation and trailing whitespace, blind to its position, changed by any edit to its code" {
    const text = "const a = y;\nconst b = 4096;\n    const b = 4096;  \r\nconst b = 4097;\nconst c = 4096;";
    const at = std.mem.indexOf(u8, text, "4096").?;
    const b = lineHash(text, at);
    try testing.expectEqual(b, lineHash(text, std.mem.indexOfPos(u8, text, at + 1, "4096").?));
    try testing.expect(b != lineHash(text, std.mem.indexOf(u8, text, "4097").?));
    try testing.expect(b != lineHash(text, std.mem.lastIndexOf(u8, text, "4096").?));
    var v = try scan(testing.allocator, "const a = y;\n\nconst b = 4096;\n");
    defer v.deinit(testing.allocator);
    try testing.expectEqual(@as(usize, 1), v.items.len);
    try testing.expectEqual(b, v.items[0].line_hash);
}

test "the harness name law" {
    try testing.expect(isHarnessFile("src/cli/smoke.zig"));
    try testing.expect(isHarnessFile("src/cli/bench_smoke.zig"));
    try testing.expect(isHarnessFile("src/data/write_e2e_linux.zig"));
    try testing.expect(isHarnessFile("src/ops/nvmet/fidelity.zig"));
    try testing.expect(isHarnessFile("src/meta/journal_core_model.zig"));
    try testing.expect(isHarnessFile("src/core/testkit.zig"));
    try testing.expect(isHarnessFile("src/sync/testing.zig"));
    try testing.expect(!isHarnessFile("src/core/knobs.zig"));
    try testing.expect(!isHarnessFile("src/data/layout.zig"));
}
