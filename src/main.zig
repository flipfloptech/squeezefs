//! The one CLI binary (charter §8): `format`, `mount` (`--daemon
//! --supervise --log-file`), `umount`, `status`, `--version`. The FIRST
//! thing main does — before argument parsing, before any volume is opened
//! or mounted — is the knob gate: a malformed `SQUEEZEFS_*` knob refuses
//! the process naming every offender (charter §1.6). Then `cli.run`.

const std = @import("std");
const squeezefs = @import("squeezefs");
const build_options = @import("build_options");

/// `std.log` goes to stderr until `mount --daemon --log-file` installs the
/// file sink (`cli/log_sink.zig`).
pub const std_options: std.Options = .{ .logFn = squeezefs.cli.log_sink.logFn };

/// The build identity this binary names (charter §11): train, commit, profile.
const version: squeezefs.Version = .{
    .train = build_options.train,
    .git_commit = build_options.git_commit,
    .profile = build_options.profile,
};

fn writeVersion(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try version.write(w);
}

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
    const code = squeezefs.cli.run(.{
        .gpa = gpa,
        .io = io,
        .env = init.environ_map,
        .argv = args,
        .stdout = stdout,
        .stderr = stderr,
        .version = writeVersion,
    });
    try stdout.flush();
    try stderr.flush();
    if (code != 0) std.process.exit(code);
}
