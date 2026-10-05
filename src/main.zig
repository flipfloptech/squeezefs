//! The one CLI binary (charter §8). Subcommands land with their rungs; today:
//! `--version`. The FIRST thing main does — before argument parsing, before
//! any volume is opened or mounted — is the knob gate: a malformed
//! `SQUEEZEFS_*` knob refuses the process naming every offender (charter §1.6).

const std = @import("std");
const squeezefs = @import("squeezefs");

pub fn main(init: std.process.Init) !void {
    const io = init.io;
    const gpa = init.gpa;

    var stderr_buf: [4096]u8 = undefined;
    var stderr_writer: std.Io.File.Writer = .init(.stderr(), io, &stderr_buf);
    const stderr = &stderr_writer.interface;

    // ---- the knob gate ----
    if (try squeezefs.knobs.refusalReport(gpa, init.environ_map, stderr)) |report| {
        defer gpa.free(report);
        try stderr.print("Error: {s}\n", .{report});
        try stderr.flush();
        std.process.exit(1);
    }
    try stderr.flush();

    var stdout_buf: [4096]u8 = undefined;
    var stdout_writer: std.Io.File.Writer = .init(.stdout(), io, &stdout_buf);
    const stdout = &stdout_writer.interface;

    const args = try init.minimal.args.toSlice(init.arena.allocator());
    if (args.len >= 2 and (std.mem.eql(u8, args[1], "--version") or std.mem.eql(u8, args[1], "version"))) {
        try squeezefs.version.write(stdout);
        try stdout.flush();
        return;
    }

    try stderr.writeAll(
        \\usage: squeezefs --version
        \\
        \\The filesystem verbs (format, mount, umount, status, fsck, …) land with
        \\their delivery rungs; see docs/charter.md §13.
        \\
    );
    try stderr.flush();
    std.process.exit(2);
}
