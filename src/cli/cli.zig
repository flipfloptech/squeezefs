//! The one CLI binary's verbs (charter §8): `format`, `mount` (`--daemon
//! --supervise --log-file`), `umount`, `status`, `clients`, `df`, `bench`,
//! `clone`, `tune`, `config`, `claim clear`, `--version`.
//! `src/main.zig` runs the knob gate first (charter §1.6) and then hands
//! argv to `run`.
//!
//! Portable here: the grammar (`args.zig`), the ONE URI parser (`uri.zig`),
//! the log sink (`log_sink.zig`) and the supervisor's pure state machine
//! (`supervisor.zig`). The verbs that touch a volume or a mount are Linux
//! only (charter §1.2) and gated, never stubbed.

const std = @import("std");
const builtin = @import("builtin");
const is_linux = builtin.os.tag == .linux;

pub const args = @import("args.zig");
pub const uri = @import("uri.zig");
pub const log_sink = @import("log_sink.zig");
pub const supervisor = @import("supervisor.zig");

pub const format = if (is_linux) @import("format.zig") else struct {};
pub const status = if (is_linux) @import("status.zig") else struct {};
pub const umount = if (is_linux) @import("umount.zig") else struct {};
pub const holders = if (is_linux) @import("holders.zig") else struct {};
pub const fusermount = if (is_linux) @import("fusermount.zig") else struct {};
pub const daemon = if (is_linux) @import("daemon.zig") else struct {};
pub const mount = if (is_linux) @import("mount.zig") else struct {};
pub const smoke = if (is_linux) @import("smoke.zig") else struct {};
pub const target = if (is_linux) @import("target.zig") else struct {};
pub const offline = if (is_linux) @import("offline.zig") else struct {};
pub const clients = if (is_linux) @import("clients.zig") else struct {};
pub const claim = if (is_linux) @import("claim.zig") else struct {};
pub const appenders = if (is_linux) @import("appenders.zig") else struct {};
pub const df = if (is_linux) @import("df.zig") else struct {};
pub const clone = if (is_linux) @import("clone.zig") else struct {};
pub const config_verb = if (is_linux) @import("config_verb.zig") else struct {};
pub const volume = if (is_linux) @import("volume.zig") else struct {};
/// Portable `merge-reports`; the offline/online harnesses are Linux inside.
pub const fsck_verb = @import("fsck.zig");
// @anchor:ops_jobs_cli — the job fabric and defrag verbs (rung 8).
pub const job = if (is_linux) @import("job.zig") else struct {};
pub const defrag_verb = if (is_linux) @import("defrag.zig") else struct {};
/// Portable planning + a Linux engine (its own gate inside).
pub const tune = @import("tune.zig");
pub const bench = @import("bench.zig");
/// The committed microbench baseline's pure half (folding, thresholds,
/// verdict, codecs); the rig is `bench_baseline_rig.zig`.
pub const bench_baseline = @import("bench_baseline.zig");
/// The rung-8 operations verbs live under `src/ops/` (Linux: the kernel
/// nvmet target + the initiator; portable: the key store's contracts).
pub const nvmet = if (is_linux) @import("../ops/nvmet.zig") else struct {};
pub const keys = @import("../ops/keys.zig");
pub const bench_smoke = if (is_linux) @import("bench_smoke.zig") else struct {};
/// The baseline rig (`zig build bench-baseline` / `bench-compare`; its
/// tests are filtered steps, like the smoke's).
pub const bench_baseline_rig = if (is_linux) @import("bench_baseline_rig.zig") else struct {};

/// Exit codes (charter §8; `sysexits`-free, three outcomes): 0 success,
/// 1 a refused or failed verb (the reason on stderr), 2 a usage error.
pub const exit_ok: u8 = 0;
pub const exit_failed: u8 = 1;
pub const exit_usage: u8 = 2;

/// The build identity (charter §11): train, commit, profile — what
/// `.stats` and `bench` rows print.
pub const Build = struct {
    train: []const u8 = "",
    commit: []const u8 = "",
    profile: []const u8 = "",
};

/// What a verb needs from the process.
pub const Process = struct {
    gpa: std.mem.Allocator,
    io: std.Io,
    env: *const std.process.Environ.Map,
    argv: []const []const u8,
    stdout: *std.Io.Writer,
    stderr: *std.Io.Writer,
    version: *const fn (*std.Io.Writer) std.Io.Writer.Error!void,
    build: Build = .{},
};

/// Parse and dispatch; returns the exit code. A usage refusal prints the
/// diagnostic and the verb's usage on stderr.
pub fn run(p: Process) u8 {
    var diag: args.Refusal = undefined;
    const verb = args.parse(p.argv, &diag) catch {
        p.stderr.print("squeezefs: {f}\n{s}", .{ diag, args.usage(verbOf(p.argv)) }) catch {};
        return exit_usage;
    };
    switch (verb) {
        .version => {
            p.version(p.stdout) catch return exit_failed;
            return exit_ok;
        },
        .help => {
            p.stdout.writeAll(args.usage(null)) catch return exit_failed;
            return exit_ok;
        },
        .format => |f| return if (is_linux) format.run(p, f) else linuxOnly(p, "format"),
        .mount => |m| return if (is_linux) mount.run(p, m) else linuxOnly(p, "mount"),
        .umount => |u| return if (is_linux) umount.run(p, u) else linuxOnly(p, "umount"),
        .status => |s| return if (is_linux) status.run(p, s) else linuxOnly(p, "status"),
        .clients => |c| return if (is_linux) clients.run(p, c) else linuxOnly(p, "clients"),
        .df => |d| return if (is_linux) df.run(p, d) else linuxOnly(p, "df"),
        .bench => |b| return bench.run(p, b),
        .clone => |c| return if (is_linux) clone.run(p, c) else linuxOnly(p, "clone"),
        .tune => |t| return tune.run(p, t),
        .config => |c| return if (is_linux) config_verb.run(p, c) else linuxOnly(p, "config"),
        .claim_clear => |c| return if (is_linux) claim.run(p, c) else linuxOnly(p, "claim clear"),
        .appenders => |a| return if (is_linux) appenders.run(p, a) else linuxOnly(p, "appenders"),
        .appender_clear => |a| return if (is_linux) appenders.runClear(p, a) else linuxOnly(p, "appender clear"),
        .volume => |v| return if (is_linux) volume.run(p, v) else linuxOnly(p, "volume"),
        .fsck => |f| return fsck_verb.run(p, f),
        .fsck_merge => |m| return fsck_verb.runMerge(p, m),
        .job => |j| return if (is_linux) job.run(p, j) else linuxOnly(p, "job"),
        .defrag => |d| return if (is_linux) defrag_verb.run(p, d) else linuxOnly(p, "defrag"),
        .nvmeof => |n| return if (is_linux) nvmet.run.run(p, n) else linuxOnly(p, "nvmeof"),
        .storage => |s| return if (is_linux) nvmet.run.runStorage(p, s) else linuxOnly(p, "storage"),
        .key => |k| return if (is_linux) keys.linux_verbs.run(p, k) else linuxOnly(p, "key"),
    }
}

/// The verb word of argv, for the usage line of a refusal.
fn verbOf(argv: []const []const u8) ?std.meta.Tag(args.Verb) {
    if (argv.len < 2) return null;
    return std.meta.stringToEnum(std.meta.Tag(args.Verb), argv[1]);
}

/// The dev box builds the binary for `--version`; every volume verb is
/// Linux only (charter §1.2).
fn linuxOnly(p: Process, verb: []const u8) u8 {
    p.stderr.print("squeezefs {s}: Linux only (charter §1.2) — this host runs the portable test suite and `--version`\n", .{verb}) catch {};
    return exit_failed;
}

test {
    _ = args;
    _ = uri;
    _ = log_sink;
    _ = supervisor;
    _ = tune;
    _ = bench;
    _ = bench_baseline;
    _ = fsck_verb;
    if (is_linux) {
        _ = format;
        _ = status;
        _ = umount;
        _ = holders;
        _ = fusermount;
        _ = daemon;
        _ = mount;
        _ = smoke;
        _ = target;
        _ = offline;
        _ = clients;
        _ = claim;
        _ = appenders;
        _ = df;
        _ = clone;
        _ = config_verb;
        _ = volume;
        _ = job;
        _ = defrag_verb;
        _ = bench_smoke;
        _ = bench_baseline_rig;
    }
}

const testing = std.testing;

test "run: a usage refusal prints the diagnostic and the verb's usage, exit 2; --version prints through the injected writer, exit 0" {
    var out: std.Io.Writer.Allocating = .init(testing.allocator);
    defer out.deinit();
    var err: std.Io.Writer.Allocating = .init(testing.allocator);
    defer err.deinit();
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    const V = struct {
        fn write(w: *std.Io.Writer) std.Io.Writer.Error!void {
            try w.writeAll("squeezefs test-train\n");
        }
    };
    var p: Process = .{ .gpa = testing.allocator, .io = testing.io, .env = &env, .argv = &.{ "squeezefs", "mount", "sqmeta:///m" }, .stdout = &out.writer, .stderr = &err.writer, .version = V.write };
    try testing.expectEqual(exit_usage, run(p));
    try testing.expect(std.mem.indexOf(u8, err.written(), "mount: missing the mountpoint") != null);
    try testing.expect(std.mem.indexOf(u8, err.written(), "usage: squeezefs mount") != null);
    p.argv = &.{ "squeezefs", "--version" };
    try testing.expectEqual(exit_ok, run(p));
    try testing.expectEqualStrings("squeezefs test-train\n", out.written());
    p.argv = &.{"squeezefs"};
    try testing.expectEqual(exit_usage, run(p));
}
