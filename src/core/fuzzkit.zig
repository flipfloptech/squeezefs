//! The ONE fuzz entry (charter §11 "Fuzz every untrusted decoder … with a
//! stable-toolchain property-test mirror"). Every `test "fuzz: …"` in the
//! tree calls `fuzzkit.fuzz` instead of `std.testing.fuzz` (the census rail
//! in `tests/rails.zig` enforces it), which adds one thing to the standard
//! entry: the REPLAY seam. When the harness sets `SQUEEZEFS_TEST_FUZZ_REPLAY`
//! to a file, the body runs that file's bytes first — through the same
//! `Smith{ .in = bytes }` stream the stable-toolchain mirror uses for the
//! corpus — so the nightly campaign's crash artefact (`zig build fuzz` saves
//! it under the corpus root) reproduces under `zig build test
//! -Dtest-filter=<target>` with no code change, and the campaign's minimizer
//! can shrink it by re-running the same test on smaller candidates.
//!
//! Under the build runner's fuzzer (`--fuzz`, `builtin.fuzz`) the seam is
//! inert: the fuzzer owns the input stream and the corpus directory.

const std = @import("std");
const builtin = @import("builtin");
const knobs = @import("knobs.zig");

comptime {
    std.debug.assert(builtin.is_test);
}

const replay_knob = "SQUEEZEFS_TEST_FUZZ_REPLAY";

/// A replayed file larger than this is not a crash artefact (the campaign's
/// inputs are KiB-class); the bound keeps a mis-pointed knob from reading a
/// volume image into a test.
const replay_limit: std.Io.Limit = .limited(16 << 20);

/// `std.testing.fuzz` with the replay seam in front.
pub inline fn fuzz(
    context: anytype,
    comptime testOne: fn (context: @TypeOf(context), smith: *std.testing.Smith) anyerror!void,
    options: std.testing.FuzzInputOptions,
) anyerror!void {
    if (!builtin.fuzz) try replay(context, testOne);
    return std.testing.fuzz(context, testOne, options);
}

fn replay(
    context: anytype,
    comptime testOne: fn (context: @TypeOf(context), smith: *std.testing.Smith) anyerror!void,
) anyerror!void {
    const path = knobs.present(std.testing.environ.getPosix(replay_knob)) orelse return;
    const gpa = std.testing.allocator;
    const bytes = try std.Io.Dir.cwd().readFileAlloc(std.testing.io, path, gpa, replay_limit);
    defer gpa.free(bytes);
    std.debug.print("fuzzkit: replaying {d} bytes from {s}\n", .{ bytes.len, path });
    var smith: std.testing.Smith = .{ .in = bytes };
    try testOne(context, &smith);
}

/// A `Smith.slice` corpus entry: `u32 LE len ‖ bytes`, built at comptime so
/// the corpus points at static memory (the shape the fuzzer's `in` stream
/// and the campaign's artefacts share).
pub fn slice(comptime bytes: []const u8) []const u8 {
    const framed = comptime blk: {
        var len: [4]u8 = undefined;
        std.mem.writeInt(u32, &len, bytes.len, .little);
        break :blk len ++ bytes[0..bytes.len].*;
    };
    return &framed;
}

test "the replay knob is a registered harness row" {
    try std.testing.expectEqual(knobs.Class.harness, knobs.lookup(replay_knob).?.class);
}

test "slice frames a corpus entry as u32 LE len ‖ bytes, and a Smith over it yields the bytes" {
    const entry = slice(&.{ 7, 8, 9 });
    try std.testing.expectEqualSlices(u8, &.{ 3, 0, 0, 0, 7, 8, 9 }, entry);
    var smith: std.testing.Smith = .{ .in = entry };
    var buf: [16]u8 = undefined;
    const n = smith.slice(&buf);
    try std.testing.expectEqualSlices(u8, &.{ 7, 8, 9 }, buf[0..n]);
}

fn echoOne(_: void, smith: *std.testing.Smith) !void {
    var buf: [64]u8 = undefined;
    _ = smith.slice(&buf);
}

test "fuzz: the kit's own entry runs its corpus and the empty stream (the census fixture)" {
    try fuzz({}, echoOne, .{ .corpus = &.{slice(&.{ 1, 2, 3 })} });
}
