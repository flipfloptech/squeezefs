//! The ONE non-allocating key reader over a `.stats`/`.config` payload
//! (`docs/porting/stats_cli.md` §7 Q12): `bench`, `umount`, `status`, the
//! CLI's mountpoint resolution and the smoke all read the surface by KEY
//! NAME — never by path, never by parsing the whole document into a tree.
//! Gauge names are unique across the families by the registry's law, so
//! the first `"key":` occurrence outside a string is the value.
//!
//! Portable; pure over bytes.

const std = @import("std");

/// The byte offset just past `"key":` (outside any string value), or null.
fn valueStart(text: []const u8, key: []const u8) ?usize {
    var i: usize = 0;
    while (i < text.len) {
        if (text[i] != '"') {
            i += 1;
            continue;
        }
        // A string token: a key iff followed by ':'.
        const start = i + 1;
        var j = start;
        var esc = false;
        while (j < text.len) : (j += 1) {
            if (esc) {
                esc = false;
                continue;
            }
            if (text[j] == '\\') {
                esc = true;
                continue;
            }
            if (text[j] == '"') break;
        }
        if (j >= text.len) return null;
        const word = text[start..j];
        i = j + 1;
        if (i < text.len and text[i] == ':') {
            if (std.mem.eql(u8, word, key)) return i + 1;
            i += 1;
            // Skip the value's leading string so a string VALUE equal to a
            // key name is never mistaken for a key.
            if (i < text.len and text[i] == '"') {
                var k = i + 1;
                var e2 = false;
                while (k < text.len) : (k += 1) {
                    if (e2) {
                        e2 = false;
                        continue;
                    }
                    if (text[k] == '\\') {
                        e2 = true;
                        continue;
                    }
                    if (text[k] == '"') break;
                }
                i = @min(text.len, k + 1);
            }
        }
    }
    return null;
}

/// The unsigned integer value of `key`, or null (absent, or not a number).
pub fn number(text: []const u8, key: []const u8) ?u64 {
    const at = valueStart(text, key) orelse return null;
    var end = at;
    while (end < text.len and text[end] >= '0' and text[end] <= '9') : (end += 1) {}
    if (end == at) return null;
    return std.fmt.parseInt(u64, text[at..end], 10) catch null;
}

/// The boolean value of `key`, or null.
pub fn boolean(text: []const u8, key: []const u8) ?bool {
    const at = valueStart(text, key) orelse return null;
    if (std.mem.startsWith(u8, text[at..], "true")) return true;
    if (std.mem.startsWith(u8, text[at..], "false")) return false;
    return null;
}

/// The string value of `key` (the raw bytes between the quotes; escapes
/// are left as written — the keys this reader serves are URIs and paths,
/// which the writer never escapes), or null.
pub fn string(text: []const u8, key: []const u8) ?[]const u8 {
    const at = valueStart(text, key) orelse return null;
    if (at >= text.len or text[at] != '"') return null;
    const start = at + 1;
    var j = start;
    while (j < text.len and text[j] != '"') : (j += 1) {
        if (text[j] == '\\') j += 1;
    }
    if (j >= text.len) return null;
    return text[start..j];
}

/// The OBJECT value of `key` as its raw `{…}` bytes (string-aware brace
/// matching), or null — the one way to read a histogram's `count`/`sum_ns`
/// (every histogram shares those field names, so `number` alone would
/// answer the first histogram in the document).
pub fn object(text: []const u8, key: []const u8) ?[]const u8 {
    const at = valueStart(text, key) orelse return null;
    if (at >= text.len or text[at] != '{') return null;
    var depth: usize = 0;
    var i = at;
    var in_str = false;
    var esc = false;
    while (i < text.len) : (i += 1) {
        const c = text[i];
        if (in_str) {
            if (esc) {
                esc = false;
            } else if (c == '\\') {
                esc = true;
            } else if (c == '"') in_str = false;
            continue;
        }
        switch (c) {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if (depth == 0) return text[at .. i + 1];
            },
            else => {},
        }
    }
    return null;
}

const testing = std.testing;

test "object by key name: the raw braces of the value, string-aware; a histogram's own count/sum_ns are then read inside it" {
    const doc =
        \\{"h1":{"count":3,"sum_ns":30,"buckets":{"<=1us":1}},"h2":{"count":5,"sum_ns":"}{",  "x":{"y":1}},"n":4}
    ;
    const h2 = object(doc, "h2").?;
    try testing.expectEqualStrings("{\"count\":5,\"sum_ns\":\"}{\",  \"x\":{\"y\":1}}", h2);
    try testing.expectEqual(@as(?u64, 5), number(h2, "count"));
    try testing.expectEqual(@as(?u64, 30), number(object(doc, "h1").?, "sum_ns"));
    try testing.expect(object(doc, "n") == null);
    try testing.expect(object(doc, "missing") == null);
}

test "number/boolean/string by key name: the first key occurrence wins, a string value equal to a key name is not a key, nested objects are transparent, absent is null" {
    const doc =
        \\{"a":{"fuse_ops":12,"name":"fuse_ops","flag":true},"b":{"fuse_ops":99,"inner":{"deep":7}},"uri":"sqmeta:///dev/m,/dev/n","neg":-3,"esc":"a\"b"}
    ;
    try testing.expectEqual(@as(?u64, 12), number(doc, "fuse_ops"));
    try testing.expectEqual(@as(?u64, 7), number(doc, "deep"));
    try testing.expectEqual(@as(?bool, true), boolean(doc, "flag"));
    try testing.expectEqualStrings("sqmeta:///dev/m,/dev/n", string(doc, "uri").?);
    try testing.expectEqualStrings("fuse_ops", string(doc, "name").?);
    try testing.expect(number(doc, "missing") == null);
    try testing.expect(number(doc, "neg") == null);
    try testing.expect(number(doc, "name") == null);
    try testing.expect(boolean(doc, "fuse_ops") == null);
    try testing.expect(string(doc, "fuse_ops") == null);
    try testing.expectEqualStrings("a\\\"b", string(doc, "esc").?);
    // A truncated document never reads past its end.
    try testing.expect(number(doc[0..10], "fuse_ops") == null);
    try testing.expect(string("{\"k\":\"unterminated", "k") == null);
}
