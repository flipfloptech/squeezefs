//! `squeezefs tune [--apply]` (charter §8; `docs/porting/stats_cli.md`
//! §4.2, §7 Q11): the client node's SYSCTL tuner — not a knob retune (knobs
//! are read once at mount, charter §1.6). Check mode by default: every
//! parameter printed with its current value, its target and its WHY;
//! `--apply` (root) writes each one and REPORTS the outcome; the exit code
//! is 1 if any apply failed (never a dropped error, never a silent 0).
//!
//! Two laws on the live FUSE connections: RAISE-ONLY (`max_background` /
//! `congestion_threshold` are negotiated at INIT to the ring capacity,
//! often above the floor — `tune` must never LOWER a live connection) and
//! `read_ahead_kb = 0` on each connection's bdi (the daemon prefetches by
//! its own stream classifier; kernel readahead doubles the device reads).
//! Linux only; the planning is pure and tested on every host.

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const cli = @import("cli.zig");
const args = @import("args.zig");

/// A raise-only floor or an exact target.
pub const Law = enum { exact, raise_only };

pub const Param = struct {
    /// The procfs/sysfs path (connection params carry `<id>` substituted).
    path: []const u8,
    name: []const u8,
    target: u64,
    law: Law,
    why: []const u8,
};

/// The sysctl table with the WHY on every line.
pub const sysctls = [_]Param{
    .{ .path = "/proc/sys/vm/dirty_ratio", .name = "vm.dirty_ratio", .target = 40, .law = .exact, .why = "a write-through FUSE mount with max_background depth needs dirty-page headroom ≥ ring capacity × max_write before the kernel throttles writers; 40 % is that headroom on a 64 GiB client" },
    .{ .path = "/proc/sys/vm/dirty_background_ratio", .name = "vm.dirty_background_ratio", .target = 10, .law = .exact, .why = "background writeback starts at a quarter of the throttle line so flushers run ahead of the throttle instead of at it" },
    .{ .path = "/proc/sys/net/core/rmem_max", .name = "net.core.rmem_max", .target = 64 << 20, .law = .exact, .why = "nvme-tcp socket receive buffers must hold a 4 MiB block in flight per queue; 64 MiB covers the 16-queue association" },
    .{ .path = "/proc/sys/net/core/wmem_max", .name = "net.core.wmem_max", .target = 64 << 20, .law = .exact, .why = "the send side of the same association" },
};

/// `/sys/fs/fuse/connections/<id>/…` params (raise-only).
pub const connection_params = [_]Param{
    .{ .path = "max_background", .name = "fuse.max_background", .target = 256, .law = .raise_only, .why = "the kernel's in-flight async request bound; the mount negotiates it to the ring capacity at INIT (often > 256) — raise-only so tune never lowers a live connection" },
    .{ .path = "congestion_threshold", .name = "fuse.congestion_threshold", .target = 192, .law = .raise_only, .why = "3/4 of max_background: writeback is throttled before the ring is full, never after" },
};

/// `/sys/class/bdi/0:<id>/read_ahead_kb` (exact).
pub const read_ahead: Param = .{ .path = "read_ahead_kb", .name = "bdi.read_ahead_kb", .target = 0, .law = .exact, .why = "the daemon prefetches by its own stream classifier; kernel readahead doubles the device reads" };

pub const Verdict = enum {
    /// Already at (or, raise-only, above) the target.
    ok,
    /// Would change.
    change,
    /// Unreadable (absent on this kernel / not a FUSE mount of ours).
    unreadable,
};

/// The pure plan: what `--apply` would write.
pub fn verdict(p: Param, current: ?u64) Verdict {
    const cur = current orelse return .unreadable;
    return switch (p.law) {
        .exact => if (cur == p.target) .ok else .change,
        .raise_only => if (cur >= p.target) .ok else .change,
    };
}

pub fn readU64(io: Io, path: []const u8) ?u64 {
    var buf: [64]u8 = undefined;
    const text = Io.Dir.cwd().readFile(io, path, &buf) catch return null;
    return std.fmt.parseInt(u64, std.mem.trim(u8, text, " \t\r\n"), 10) catch null;
}

fn writeU64(io: Io, path: []const u8, v: u64) bool {
    const f = Io.Dir.cwd().openFile(io, path, .{ .mode = .write_only }) catch return false;
    defer f.close(io);
    var buf: [32]u8 = undefined;
    const text = std.fmt.bufPrint(&buf, "{d}\n", .{v}) catch return false;
    f.writeStreamingAll(io, text) catch return false;
    return true;
}

const Outcome = struct { changed: usize = 0, failed: usize = 0 };

fn handle(p: cli.Process, param: Param, path: []const u8, apply: bool, out: *Outcome) void {
    const current = readU64(p.io, path);
    const v = verdict(param, current);
    switch (v) {
        .unreadable => p.stdout.print("{s:<28} {s:<10} -> {d:<10} unreadable ({s})\n", .{ param.name, "?", param.target, path }) catch {},
        .ok => p.stdout.print("{s:<28} {d:<10} -> {d:<10} ok{s}\n", .{ param.name, current.?, param.target, if (param.law == .raise_only) " (raise-only; at or above)" else "" }) catch {},
        .change => {
            if (apply) {
                if (writeU64(p.io, path, param.target)) {
                    out.changed += 1;
                    p.stdout.print("{s:<28} {d:<10} -> {d:<10} APPLIED\n", .{ param.name, current.?, param.target }) catch {};
                } else {
                    out.failed += 1;
                    p.stdout.print("{s:<28} {d:<10} -> {d:<10} FAILED to write {s} (root?)\n", .{ param.name, current.?, param.target, path }) catch {};
                }
            } else {
                p.stdout.print("{s:<28} {d:<10} -> {d:<10} would change\n", .{ param.name, current.?, param.target }) catch {};
            }
        },
    }
    p.stdout.print("    why: {s}\n", .{param.why}) catch {};
}

pub fn run(p: cli.Process, t: args.Tune) u8 {
    if (builtin.os.tag != .linux) {
        p.stderr.writeAll("squeezefs tune: Linux only (charter §1.2)\n") catch {};
        return cli.exit_failed;
    }
    const root = std.os.linux.geteuid() == 0;
    if (t.apply and !root) {
        p.stderr.writeAll("squeezefs tune: --apply needs root (the sysctls and the fuse connection params are root-writable); run without --apply to check\n") catch {};
        return cli.exit_failed;
    }
    p.stdout.print("squeezefs tune: {s}\n", .{if (t.apply) "applying" else "check only (pass --apply as root to write)"}) catch {};
    var out: Outcome = .{};
    for (sysctls) |s| handle(p, s, s.path, t.apply, &out);
    // The live FUSE connections and their bdis.
    var dir = Io.Dir.cwd().openDir(p.io, "/sys/fs/fuse/connections", .{ .iterate = true }) catch {
        p.stdout.writeAll("fuse connections: none (/sys/fs/fuse/connections absent — fusectl not mounted)\n") catch {};
        return finish(p, out);
    };
    defer dir.close(p.io);
    var it = dir.iterate();
    var conns: usize = 0;
    while (it.next(p.io) catch null) |entry| {
        if (entry.kind != .directory) continue;
        conns += 1;
        for (connection_params) |cp| {
            var buf: [256]u8 = undefined;
            const path = std.fmt.bufPrint(&buf, "/sys/fs/fuse/connections/{s}/{s}", .{ entry.name, cp.path }) catch continue;
            var named = cp;
            named.name = std.fmt.bufPrint(buf[200..], "fuse{s}.{s}", .{ entry.name, cp.path }) catch cp.name;
            handle(p, named, path, t.apply, &out);
        }
        var bbuf: [256]u8 = undefined;
        const bdi = std.fmt.bufPrint(&bbuf, "/sys/class/bdi/0:{s}/{s}", .{ entry.name, read_ahead.path }) catch continue;
        var ra = read_ahead;
        ra.name = std.fmt.bufPrint(bbuf[200..], "bdi0:{s}.read_ahead_kb", .{entry.name}) catch read_ahead.name;
        handle(p, ra, bdi, t.apply, &out);
    }
    if (conns == 0) p.stdout.writeAll("fuse connections: none mounted\n") catch {};
    return finish(p, out);
}

fn finish(p: cli.Process, out: Outcome) u8 {
    p.stdout.print("tune: {d} changed, {d} failed\n", .{ out.changed, out.failed }) catch {};
    p.stdout.flush() catch {};
    return if (out.failed != 0) cli.exit_failed else cli.exit_ok;
}

const testing = std.testing;

test "the plan: exact targets change unless equal; raise-only floors never lower a live connection; an unreadable parameter is reported, not written; every target carries a WHY" {
    try testing.expectEqual(Verdict.ok, verdict(sysctls[0], 40));
    try testing.expectEqual(Verdict.change, verdict(sysctls[0], 20));
    try testing.expectEqual(Verdict.change, verdict(sysctls[0], 60));
    try testing.expectEqual(Verdict.ok, verdict(connection_params[0], 1024));
    try testing.expectEqual(Verdict.ok, verdict(connection_params[0], 256));
    try testing.expectEqual(Verdict.change, verdict(connection_params[0], 12));
    try testing.expectEqual(Verdict.unreadable, verdict(read_ahead, null));
    try testing.expectEqual(Verdict.ok, verdict(read_ahead, 0));
    try testing.expectEqual(Verdict.change, verdict(read_ahead, 128));
    for (sysctls) |s| try testing.expect(s.why.len > 20);
    for (connection_params) |c| try testing.expect(c.why.len > 20 and c.law == .raise_only);
    try testing.expectEqual(@as(u64, 3 * 256 / 4), connection_params[1].target);
    try testing.expect(readU64(testing.io, "/definitely/not/here") == null);
}
