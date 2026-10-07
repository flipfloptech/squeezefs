//! THE BENCH SMOKE (charter §11 "per-commit … bench smoke (one iteration
//! of every microbench)"; `docs/porting/stats_cli.md` §3.5): `zig build
//! bench-smoke` runs the built binary as a child — a format, a mount, then
//! `squeezefs bench --smoke <mountpoint>` (one iteration of every workload
//! on the 2 × 1 × 4 MiB shape, ≤ 16 MiB of real data), the rows parsed
//! for the measurement-law fields and the must-stay-0 census, then the
//! umount and the work directory removed. Venue only: the test skips
//! loudly where the binary under test is not in the cwd (the unit run on
//! the dev box). Like `smoke.zig`, every path through the rig unmounts.
//! The test's name is `bench gate:` (not `…smoke:`) so the `smoke` step's
//! filter never runs it a second time; the step itself runs AFTER the
//! smoke so two mounts never share the venue.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const clock = @import("../core/clock.zig");
const json_scan = @import("../core/json_scan.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/bench_smoke is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

const testing = std.testing;

/// 256 MiB meta + two 64 MiB data volumes: the smoke shape (16 MiB) at 4
/// MiB blocks needs ≤ 8 blocks; the rest is headroom for the packs.
const meta_len: u64 = 256 << 20;
const data_len: u64 = 64 << 20;
const ready_wait_ns: u64 = 60 * std.time.ns_per_s;

const Rig = struct {
    gpa: std.mem.Allocator,
    io: Io,
    bin: [:0]u8,
    work: [:0]u8,
    mp: [:0]u8,
    meta_uri: []u8,
    data_uri: []u8,
    log_path: [:0]u8,
    log_file: ?Io.File = null,
    child: ?std.process.Child = null,
    child_pid: linux.pid_t = 0,
    env: std.process.Environ.Map,

    fn init(gpa: std.mem.Allocator, io: Io) !*Rig {
        const self = try gpa.create(Rig);
        errdefer gpa.destroy(self);
        const pid = linux.getpid();
        _ = linux.mkdir("/build/tmp", 0o755);
        var cwd_buf: [linux.PATH_MAX]u8 = undefined;
        const n = Io.Dir.cwd().realPath(io, &cwd_buf) catch return error.NoCwd;
        self.* = .{
            .gpa = gpa,
            .io = io,
            .bin = try std.fmt.allocPrintSentinel(gpa, "{s}/squeezefs", .{cwd_buf[0..n]}, 0),
            .work = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/bench-smoke-{d}", .{pid}, 0),
            .mp = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/bench-mnt-{d}", .{pid}, 0),
            .meta_uri = try std.fmt.allocPrint(gpa, "sqmeta:///build/tmp/bench-smoke-{d}/vol.img", .{pid}),
            .data_uri = try std.fmt.allocPrint(gpa, "sqdata:///build/tmp/bench-smoke-{d}/d0.img,/build/tmp/bench-smoke-{d}/d1.img", .{ pid, pid }),
            .log_path = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/bench-smoke-{d}/daemon.log", .{pid}, 0),
            .env = std.process.Environ.Map.init(gpa),
        };
        try self.env.put("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
        _ = linux.mkdir(self.work.ptr, 0o755);
        _ = linux.mkdir(self.mp.ptr, 0o755);
        return self;
    }

    fn deinit(self: *Rig) void {
        if (self.child) |*c| {
            _ = linux.kill(self.child_pid, .KILL);
            _ = c.wait(self.io) catch {};
        }
        _ = linux.umount2(self.mp.ptr, linux.MNT.DETACH);
        if (self.log_file) |f| f.close(self.io);
        _ = linux.rmdir(self.mp.ptr);
        Io.Dir.cwd().deleteTree(self.io, self.work) catch {};
        self.env.deinit();
        self.gpa.free(self.bin);
        self.gpa.free(self.work);
        self.gpa.free(self.mp);
        self.gpa.free(self.meta_uri);
        self.gpa.free(self.data_uri);
        self.gpa.free(self.log_path);
        self.gpa.destroy(self);
    }

    fn sparse(self: *Rig, name: []const u8, len: u64) !void {
        var buf: [linux.PATH_MAX]u8 = undefined;
        const path = try std.fmt.bufPrint(&buf, "{s}/{s}", .{ self.work, name });
        const f = try Io.Dir.cwd().createFile(self.io, path, .{ .truncate = true });
        defer f.close(self.io);
        try f.setLength(self.io, len);
    }

    fn squeezefs(self: *Rig, a: []const []const u8) !std.process.RunResult {
        var argv: [16][]const u8 = undefined;
        argv[0] = self.bin;
        for (a, 0..) |x, i| argv[i + 1] = x;
        return std.process.run(self.gpa, self.io, .{ .argv = argv[0 .. a.len + 1], .environ_map = &self.env });
    }

    fn readLog(self: *Rig) ![]u8 {
        return Io.Dir.cwd().readFileAlloc(self.io, self.log_path, self.gpa, .limited(16 << 20));
    }

    fn awaitLog(self: *Rig, needle: []const u8) !void {
        const deadline = clock.monotonicNs() + ready_wait_ns;
        while (true) {
            const text = try self.readLog();
            defer self.gpa.free(text);
            if (std.mem.indexOf(u8, text, needle) != null) return;
            if (clock.monotonicNs() >= deadline) {
                std.debug.print("\n--- bench smoke: timed out waiting for '{s}' ---\n{s}\n", .{ needle, text });
                return error.Timeout;
            }
            var ts: linux.timespec = .{ .sec = 0, .nsec = 20 * std.time.ns_per_ms };
            _ = linux.nanosleep(&ts, null);
        }
    }

    fn spawnMount(self: *Rig) !void {
        self.log_file = try Io.Dir.cwd().createFile(self.io, self.log_path, .{ .truncate = false });
        self.child = try std.process.spawn(self.io, .{ .argv = &.{ self.bin, "mount", self.meta_uri, self.mp }, .environ_map = &self.env, .stdin = .ignore, .stdout = .{ .file = self.log_file.? }, .stderr = .{ .file = self.log_file.? } });
        self.child_pid = self.child.?.id.?;
        try self.awaitLog("ready: ");
    }

    fn umount(self: *Rig) !void {
        const r = try self.squeezefs(&.{ "umount", self.mp });
        defer self.gpa.free(r.stdout);
        defer self.gpa.free(r.stderr);
        if (!r.term.success()) std.debug.print("\numount: {s}\n", .{r.stderr});
        try testing.expect(r.term.success());
        var c = self.child.?;
        self.child = null;
        _ = try c.wait(self.io);
    }
};

fn venueAvailable(io: Io) bool {
    Io.Dir.cwd().access(io, "squeezefs", .{}) catch return false;
    Io.Dir.cwd().access(io, "/dev/fuse", .{}) catch return false;
    return true;
}

test "bench gate: format → mount → `squeezefs bench --smoke --json` runs every workload once on the smoke shape (rows carry the measurement-law fields; invariant_tripwires stays 0; the dataset is deleted) → umount" {
    if (!venueAvailable(testing.io)) {
        std.debug.print("bench-smoke: skipped — no ./squeezefs binary or /dev/fuse here (the Linux venue runs it: `scripts/venue zig build bench-smoke`)\n", .{});
        return;
    }
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, testing.io);
    defer rig.deinit();
    try rig.sparse("vol.img", meta_len);
    try rig.sparse("d0.img", data_len);
    try rig.sparse("d1.img", data_len);
    {
        const r = try rig.squeezefs(&.{ "format", rig.meta_uri, rig.data_uri });
        defer gpa.free(r.stdout);
        defer gpa.free(r.stderr);
        if (!r.term.success()) std.debug.print("\nformat: {s}\n", .{r.stderr});
        try testing.expect(r.term.success());
    }
    try rig.spawnMount();
    const r = try rig.squeezefs(&.{ "bench", "--smoke", "--json", rig.mp });
    defer gpa.free(r.stdout);
    defer gpa.free(r.stderr);
    if (!r.term.success()) std.debug.print("\nbench --smoke: {s}\n{s}\n", .{ r.stdout, r.stderr });
    try testing.expect(r.term.success());
    // Six rows (write seq, read seq, read rand, write rand, stat, del), one line each.
    var lines = std.mem.tokenizeScalar(u8, r.stdout, '\n');
    var n: usize = 0;
    var saw_del = false;
    while (lines.next()) |line| {
        n += 1;
        try testing.expect(std.mem.startsWith(u8, line, "{\"pass\":\""));
        for ([_][]const u8{ "\"elapsed_ns\":", "\"ops\":", "\"bytes\":", "\"mib_per_s\":", "\"iops\":", "\"lat_ns\":{", "\"amplification_ppm\":", "\"wareq_sz\":", "\"reclaim\":{", "\"invariant_tripwires\":" }) |needle| {
            if (std.mem.indexOf(u8, line, needle) == null) {
                std.debug.print("\n--- row lacks {s}: {s} ---\n", .{ needle, line });
                return error.RowFieldMissing;
            }
        }
        try testing.expectEqual(@as(?u64, 0), json_scan.number(line, "invariant_tripwires"));
        try testing.expect(json_scan.boolean(line, "stats_present").? == true);
        // Full coverage on the smoke shape: every row completes its ops.
        try testing.expectEqual(json_scan.number(line, "expected_ops").?, json_scan.number(line, "ops").?);
        if (std.mem.startsWith(u8, line, "{\"pass\":\"del\"")) saw_del = true;
    }
    try testing.expectEqual(@as(usize, 6), n);
    try testing.expect(saw_del);
    std.debug.print("bench-smoke: {d} rows (write seq, read seq, read rand, write rand, stat, del) on the {d} × 1 × {d} MiB shape; every row at full coverage, invariant_tripwires 0\n", .{ n, 2, 4 });
    // The dataset is gone.
    var buf: [linux.PATH_MAX:0]u8 = undefined;
    const ds = try std.fmt.bufPrintSentinel(&buf, "{s}/squeezefs-bench", .{rig.mp}, 0);
    var st: linux.Statx = undefined;
    try testing.expect(linux.errno(linux.statx(linux.AT.FDCWD, ds.ptr, 0, .{ .TYPE = true }, &st)) == .NOENT);
    // `.stats` is the control inode: 0400, owner = the mount uid, not listed.
    const sp = try std.fmt.bufPrintSentinel(&buf, "{s}/.stats", .{rig.mp}, 0);
    try testing.expect(linux.errno(linux.statx(linux.AT.FDCWD, sp.ptr, 0, .{ .MODE = true, .UID = true, .SIZE = true }, &st)) == .SUCCESS);
    try testing.expectEqual(@as(u16, 0o400), st.mode & 0o7777);
    try testing.expectEqual(linux.geteuid(), st.uid);
    try testing.expect(st.size > 1024);
    try rig.umount();
}
