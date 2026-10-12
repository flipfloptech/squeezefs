//! Regenerates `findvsa_pred_bset.v5.bin` from the in-tree generator
//! (`meta/bset.findVsAFixtureImage`), which the pin test in `bset.zig`
//! compares the committed bytes against. Run from the repository root:
//!
//!     zig run --dep squeezefs -Mroot=tests/fixtures/gen_findvsa_pred_bset.zig -Msqueezefs=src/squeezefs.zig
//!
//! Only an intentional format change should ever make the pin test red;
//! regenerate, review the diff and commit the new bytes with that change.

const std = @import("std");
const bset = @import("squeezefs").meta.bset;

pub fn main(init: std.process.Init) !void {
    const gpa = init.gpa;
    const image = try bset.findVsAFixtureImage(gpa);
    defer gpa.free(image);
    try std.Io.Dir.cwd().writeFile(init.io, .{ .sub_path = bset.findvsa_fixture_path, .data = image });
    std.debug.print("{s}: {d} bytes\n", .{ bset.findvsa_fixture_path, image.len });
}
