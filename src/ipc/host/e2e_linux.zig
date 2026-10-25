//! The daemon host end to end (charter §7; spec `docs/porting/il_daemon.md`
//! §7): the built `squeezefs` as a child with `-o interception` over a
//! real set on the venue's ext4, the ONE client library IN THIS PROCESS
//! establishing a session over the real `il0` xattr, and the data ops
//! through the ring against a bound fd — read back through the KERNEL
//! mount path (write-through forced). `smoke:`-tagged: `zig build smoke`
//! runs it with the binary of this build as `./squeezefs` in the cwd;
//! anything less than root + `/dev/fuse` + `CAP_SYS_ADMIN` skips loudly.
//!
//! The daemon runs the device overlay's control arm (`SQUEEZEFS_DEVICE_
//! OVERLAY=0`): placed sever engages where ACCUMULATION is the arm — a
//! transformed volume, or the overlay lever off — and this rig pins the
//! adoption on a passthrough set that way; the W1 inline arm is
//! independent of the overlay.

const std = @import("std");
const testkit = @import("../../core/testkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;

const clock = @import("../../core/clock.zig");
const prefetch = @import("../../data/prefetch.zig");
const log = @import("../../core/log.zig");
const wire = @import("../wire.zig");
const client = @import("../client.zig");
const rendezvous = @import("../rendezvous.zig");
const Atomic = std.atomic.Value;

comptime {
    if (builtin.os.tag != .linux) @compileError("ipc/host/e2e_linux is Linux only; gate the import on builtin.os.tag");
}

const testing = std.testing;

/// The meta volume (the smoke's worked example) and two sparse data volumes.
const meta_len: u64 = 256 << 20;
const data_len: u64 = 2 << 30;
const ready_wait_ns: u64 = 60 * std.time.ns_per_s;
const exit_wait_ns: u64 = 60 * std.time.ns_per_s;
/// The default 4 MiB block: the placed stream below fills exactly one.
const BLOCK: u64 = 4 << 20;
const MiB: u64 = 1 << 20;

const Rig = struct {
    gpa: std.mem.Allocator,
    io: Io,
    bin: [:0]u8,
    work: [:0]u8,
    img: [:0]u8,
    d0: [:0]u8,
    d1: [:0]u8,
    meta_uri: []u8,
    data_uri: []u8,
    mp: [:0]u8,
    log_path: [:0]u8,
    log_file: ?Io.File = null,
    child: ?std.process.Child = null,
    child_pid: linux.pid_t = 0,
    env: std.process.Environ.Map,
    /// The mount's `-o` string (`interception`, or the direct arm's).
    mount_opts: []const u8 = "interception",
    /// Set by a test's last statement: a deinit before it is a failure's,
    /// and echoes the daemon's alert lines as the diagnostic.
    passed: bool = false,
    /// Mount `--daemon --supervise` (a row that relies on the write cache:
    /// no write cache without a supervisor, decision 0003 §BR.6). The
    /// daemon is then a grandchild: `child` stays null, `child_pid` is its
    /// ready line's, its term its supervisor's line.
    supervise: bool = false,
    supervisor_pid: linux.pid_t = 0,

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
            .work = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/il-{d}", .{pid}, 0),
            .img = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/il-{d}/vol.img", .{pid}, 0),
            .d0 = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/il-{d}/d0.img", .{pid}, 0),
            .d1 = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/il-{d}/d1.img", .{pid}, 0),
            .meta_uri = try std.fmt.allocPrint(gpa, "sqmeta:///build/tmp/il-{d}/vol.img", .{pid}),
            .data_uri = try std.fmt.allocPrint(gpa, "sqdata:///build/tmp/il-{d}/d0.img,/build/tmp/il-{d}/d1.img", .{ pid, pid }),
            .mp = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/il-mnt-{d}", .{pid}, 0),
            .log_path = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/il-{d}/daemon.log", .{pid}, 0),
            .env = std.process.Environ.Map.init(gpa),
        };
        try self.env.put("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
        // The dev box's dirty tree pairs under the seam (both sides).
        try self.env.put("SQUEEZEFS_TEST_IPC_ALLOW_DIRTY", "1");
        // The overlay's control arm (module doc).
        try self.env.put("SQUEEZEFS_DEVICE_OVERLAY", "0");
        // The reference shim's 1 MiB op class: a 4 MiB pwrite is four
        // flights under one doorbell (the derived ceiling at a 4 MiB block
        // would make it ONE flight — one chunk, one assembly, no siblings).
        try self.env.put("SQUEEZEFS_IPC_MAX_OP_BYTES", "1048576");
        _ = linux.mkdir(self.work.ptr, 0o755);
        _ = linux.mkdir(self.mp.ptr, 0o755);
        _ = linux.prctl(@backingInt(linux.PR.SET_CHILD_SUBREAPER), 1, 0, 0, 0);
        return self;
    }

    fn deinit(self: *Rig) void {
        if (self.child) |*c| {
            _ = linux.kill(self.child_pid, .KILL);
            _ = c.wait(self.io) catch {};
        }
        if (self.supervise) for ([_]linux.pid_t{ self.child_pid, self.supervisor_pid }) |p| if (p != 0 and alive(p)) {
            _ = linux.kill(p, .KILL);
        };
        if (!self.passed) self.printDaemonAlerts();
        _ = linux.umount2(self.mp.ptr, linux.MNT.DETACH);
        if (self.log_file) |f| f.close(self.io);
        _ = linux.rmdir(self.mp.ptr);
        Io.Dir.cwd().deleteTree(self.io, self.work) catch {};
        self.env.deinit();
        self.gpa.free(self.bin);
        self.gpa.free(self.work);
        self.gpa.free(self.img);
        self.gpa.free(self.d0);
        self.gpa.free(self.d1);
        self.gpa.free(self.meta_uri);
        self.gpa.free(self.data_uri);
        self.gpa.free(self.mp);
        self.gpa.free(self.log_path);
        self.gpa.destroy(self);
    }

    fn printDaemonAlerts(self: *Rig) void {
        const text = self.readLog() catch return;
        defer self.gpa.free(text);
        var it = std.mem.splitScalar(u8, text, '\n');
        var n: usize = 0;
        while (it.next()) |line| {
            if (std.mem.startsWith(u8, line, "error(") or std.mem.startsWith(u8, line, "warning(") or std.mem.indexOf(u8, line, "TRIPWIRE") != null) {
                if (n == 0) std.debug.print("\n--- daemon alerts ---\n", .{});
                std.debug.print("{s}\n", .{line[0..@min(line.len, 400)]});
                n += 1;
                if (n == 40) break;
            }
        }
    }

    fn formatSet(self: *Rig) !void {
        for ([_]struct { p: [:0]const u8, len: u64 }{ .{ .p = self.img, .len = meta_len }, .{ .p = self.d0, .len = data_len }, .{ .p = self.d1, .len = data_len } }) |v| {
            const f = try Io.Dir.cwd().createFile(self.io, v.p, .{ .truncate = true });
            defer f.close(self.io);
            try f.setLength(self.io, v.len);
        }
        const r = try std.process.run(self.gpa, self.io, .{ .argv = &.{ self.bin, "format", self.meta_uri, self.data_uri }, .environ_map = &self.env });
        defer self.gpa.free(r.stdout);
        defer self.gpa.free(r.stderr);
        if (!r.term.success()) std.debug.print("\nformat: {s}\n", .{r.stderr});
        try testing.expect(r.term.success());
    }

    fn spawnMount(self: *Rig) !void {
        std.debug.assert(self.child == null);
        if (self.log_file == null) self.log_file = try Io.Dir.cwd().createFile(self.io, self.log_path, .{ .truncate = false });
        if (self.supervise) {
            const mark = try self.logLen();
            const r = try std.process.run(self.gpa, self.io, .{ .argv = &.{ self.bin, "mount", "--daemon", "--supervise", "--log-file", self.log_path, "-o", self.mount_opts, self.meta_uri, self.mp }, .environ_map = &self.env });
            defer self.gpa.free(r.stdout);
            defer self.gpa.free(r.stderr);
            if (!r.term.success()) std.debug.print("\nsupervised mount: {s}\n", .{r.stderr});
            try testing.expect(r.term.success());
            const at = try self.awaitLog("ready: ", mark, ready_wait_ns);
            const text = try self.readLog();
            defer self.gpa.free(text);
            self.child_pid = @intCast(numberAfter(text[at..], "(pid ") orelse return error.NoPid);
            self.supervisor_pid = parentOf(self.child_pid) orelse 0;
            return;
        }
        const argv = [_][]const u8{ self.bin, "mount", "-o", self.mount_opts, self.meta_uri, self.mp };
        self.child = try std.process.spawn(self.io, .{ .argv = &argv, .environ_map = &self.env, .stdin = .ignore, .stdout = .{ .file = self.log_file.? }, .stderr = .{ .file = self.log_file.? } });
        self.child_pid = self.child.?.id.?;
    }

    fn readLog(self: *Rig) ![]u8 {
        return Io.Dir.cwd().readFileAlloc(self.io, self.log_path, self.gpa, .limited(16 << 20));
    }

    fn awaitLog(self: *Rig, needle: []const u8, from: usize, deadline_ns: u64) !usize {
        const deadline = clock.monotonicNs() + deadline_ns;
        while (true) {
            const text = try self.readLog();
            defer self.gpa.free(text);
            if (text.len > from) if (std.mem.indexOf(u8, text[from..], needle)) |i| return from + i + needle.len;
            if (self.child) |*c| if (!alive(c.id.?)) {
                std.debug.print("\n--- the daemon exited while waiting for '{s}' ---\n{s}\n", .{ needle, text });
                return error.DaemonExited;
            };
            if (clock.monotonicNs() >= deadline) {
                std.debug.print("\n--- timed out waiting for '{s}' ---\n{s}\n", .{ needle, text });
                return error.Timeout;
            }
            sleepMs(20);
        }
    }

    fn logLen(self: *Rig) !usize {
        const text = try self.readLog();
        defer self.gpa.free(text);
        return text.len;
    }

    fn umount(self: *Rig) !void {
        const r = try std.process.run(self.gpa, self.io, .{ .argv = &.{ self.bin, "umount", self.mp }, .environ_map = &self.env });
        defer self.gpa.free(r.stdout);
        defer self.gpa.free(r.stderr);
        if (!r.term.success()) std.debug.print("\numount: {s}\n", .{r.stderr});
        try testing.expect(r.term.success());
    }

    fn waitChild(self: *Rig) !std.process.Child.Term {
        if (self.supervise) {
            const deadline = clock.monotonicNs() + exit_wait_ns;
            while (alive(self.child_pid) and clock.monotonicNs() < deadline) sleepMs(20);
            if (alive(self.child_pid)) {
                std.debug.print("\n--- the daemon did not exit within {d} s; killing ---\n", .{exit_wait_ns / std.time.ns_per_s});
                _ = linux.kill(self.child_pid, .KILL);
            }
            var nb: [64]u8 = undefined;
            const at = try self.awaitLog(try std.fmt.bufPrint(&nb, "daemon {d} exited (wait status 0x", .{self.child_pid}), 0, 30 * std.time.ns_per_s);
            const text = try self.readLog();
            defer self.gpa.free(text);
            const end = std.mem.indexOfScalarPos(u8, text, at, ')') orelse return error.NoWaitStatus;
            const status = std.fmt.parseInt(u32, text[at..end], 16) catch return error.NoWaitStatus;
            if (status & 0x7f == 0) return .{ .exited = @intCast((status >> 8) & 0xff) };
            return .{ .signal = @fromBackingInt(@intCast(status & 0x7f)) };
        }
        var c = self.child.?;
        self.child = null;
        const deadline = clock.monotonicNs() + exit_wait_ns;
        while (alive(self.child_pid) and clock.monotonicNs() < deadline) sleepMs(20);
        if (alive(self.child_pid)) {
            std.debug.print("\n--- the daemon did not exit within {d} s; killing ---\n", .{exit_wait_ns / std.time.ns_per_s});
            _ = linux.kill(self.child_pid, .KILL);
        }
        return c.wait(self.io);
    }

    /// `.stats` by its absolute path, owned (procfs-style: size 0, read
    /// to EOF into one bounded buffer).
    fn stats(self: *Rig) ![]u8 {
        var pb: [160]u8 = undefined;
        const path = try std.fmt.bufPrintSentinel(&pb, "{s}/.stats", .{self.mp}, 0);
        const rc = linux.open(path, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
        if (linux.errno(rc) != .SUCCESS) return error.StatsOpen;
        const fd: linux.fd_t = @intCast(rc);
        defer _ = linux.close(fd);
        const buf = try self.gpa.alloc(u8, 1 << 20);
        errdefer self.gpa.free(buf);
        var total: usize = 0;
        while (total < buf.len) {
            const n = linux.read(fd, buf[total..].ptr, buf.len - total);
            if (linux.errno(n) != .SUCCESS) return error.StatsRead;
            if (n == 0) break;
            total += n;
        }
        return self.gpa.realloc(buf, total);
    }

    fn stat(self: *Rig, key: []const u8) !u64 {
        const text = try self.stats();
        defer self.gpa.free(text);
        var kb: [128]u8 = undefined;
        const needle = try std.fmt.bufPrint(&kb, "\"{s}\":", .{key});
        return numberAfter(text, needle) orelse error.StatMissing;
    }

    /// Bounded poll until `key == want`.
    fn awaitStat(self: *Rig, key: []const u8, want: u64) !void {
        const deadline = clock.monotonicNs() + 20 * std.time.ns_per_s;
        while (clock.monotonicNs() < deadline) {
            if ((try self.stat(key)) == want) return;
            sleepMs(50);
        }
        std.debug.print("\n--- {s} did not reach {d} (is {d}) ---\n", .{ key, want, try self.stat(key) });
        return error.StatNotReached;
    }

    fn filePath(self: *Rig, buf: []u8, name: []const u8) ![:0]const u8 {
        return std.fmt.bufPrintSentinel(buf, "{s}/{s}", .{ self.mp, name }, 0);
    }

    fn openOnMount(self: *Rig, name: []const u8, flags: linux.O) !linux.fd_t {
        var pb: [256]u8 = undefined;
        const p = try self.filePath(&pb, name);
        const rc = linux.open(p, flags, 0o644);
        if (linux.errno(rc) != .SUCCESS) return error.OpenFailed;
        return @intCast(rc);
    }

    fn il0Blob(self: *Rig) !wire.Il0Blob {
        var raw: [wire.BLOB_LEN]u8 = undefined;
        const n = linux.getxattr(self.mp.ptr, wire.IL0_XATTR, &raw, raw.len);
        if (linux.errno(n) != .SUCCESS) return error.NoBlob;
        if (n != wire.BLOB_LEN) return error.BadBlob;
        return wire.Il0Blob.decode(&raw);
    }
};

fn numberAfter(text: []const u8, key: []const u8) ?u64 {
    const i = std.mem.indexOf(u8, text, key) orelse return null;
    var j = i + key.len;
    while (j < text.len and (text[j] == ' ')) j += 1;
    var end = j;
    while (end < text.len and std.ascii.isDigit(text[end])) end += 1;
    if (end == j) return null;
    return std.fmt.parseInt(u64, text[j..end], 10) catch null;
}

/// The parent of `pid` (`/proc/<pid>/stat`'s fourth field).
fn parentOf(pid: linux.pid_t) ?linux.pid_t {
    var pb: [64]u8 = undefined;
    var buf: [512]u8 = undefined;
    const path = std.fmt.bufPrint(&pb, "/proc/{d}/stat", .{pid}) catch return null;
    const text = Io.Dir.cwd().readFile(testing.io, path, &buf) catch return null;
    const close = std.mem.lastIndexOfScalar(u8, text, ')') orelse return null;
    var it = std.mem.tokenizeScalar(u8, text[close + 1 ..], ' ');
    _ = it.next() orelse return null;
    return std.fmt.parseInt(linux.pid_t, it.next() orelse return null, 10) catch null;
}

fn alive(pid: linux.pid_t) bool {
    var pb: [64]u8 = undefined;
    var buf: [512]u8 = undefined;
    const path = std.fmt.bufPrint(&pb, "/proc/{d}/stat", .{pid}) catch return false;
    const text = Io.Dir.cwd().readFile(testing.io, path, &buf) catch return false;
    const close = std.mem.lastIndexOfScalar(u8, text, ')') orelse return false;
    if (close + 2 >= text.len) return false;
    return text[close + 2] != 'Z' and text[close + 2] != 'X';
}

fn sleepMs(ms: u64) void {
    const ts: linux.timespec = .{ .sec = @intCast(ms / 1000), .nsec = @intCast((ms % 1000) * std.time.ns_per_ms) };
    _ = linux.nanosleep(&ts, null);
}

fn venueMissing(io: Io) ?testkit.Why {
    return testkit.venue.mount(io, &.{"squeezefs"});
}

fn fuseMountsOf(gpa: std.mem.Allocator, io: Io, mp: []const u8) !usize {
    const buf = try gpa.alloc(u8, 1 << 20);
    defer gpa.free(buf);
    const text = try Io.Dir.cwd().readFile(io, "/proc/self/mounts", buf);
    var n: usize = 0;
    var it = std.mem.splitScalar(u8, text, '\n');
    while (it.next()) |line| {
        if (std.mem.indexOf(u8, line, mp) != null and std.mem.indexOf(u8, line, " fuse") != null) n += 1;
    }
    return n;
}

fn pattern(buf: []u8, base: u64) void {
    for (buf, 0..) |*b, i| b.* = @truncate((base + i) *% 2654435761 >> 7);
}

fn expectPattern(buf: []const u8, base: u64) !void {
    for (buf, 0..) |v, i| {
        const want: u8 = @truncate((base + i) *% 2654435761 >> 7);
        if (v != want) {
            std.debug.print("\n--- byte {d} (base {d}): got {x} want {x} ---\n", .{ i, base, v, want });
            return error.TestBytesDiffer;
        }
    }
}

/// The placed-sever counters (`il_daemon.md` §3.2) at one instant.
const Placement = struct {
    severs: u64,
    fallbacks: u64,
    adoptions: u64,
    refusals: u64,
    reaps: u64,

    fn read(rig: *Rig) !Placement {
        const text = try rig.stats();
        defer rig.gpa.free(text);
        return .{
            .severs = numberAfter(text, "\"ipc_placed_severs\":") orelse return error.StatMissing,
            .fallbacks = numberAfter(text, "\"ipc_placed_sever_fallbacks\":") orelse return error.StatMissing,
            .adoptions = numberAfter(text, "\"ipc_placed_adoptions\":") orelse return error.StatMissing,
            .refusals = numberAfter(text, "\"ipc_placed_adoption_refusals\":") orelse return error.StatMissing,
            .reaps = numberAfter(text, "\"ipc_placed_reaps\":") orelse return error.StatMissing,
        };
    }

    fn since(self: Placement, base: Placement) Placement {
        return .{
            .severs = self.severs - base.severs,
            .fallbacks = self.fallbacks - base.fallbacks,
            .adoptions = self.adoptions - base.adoptions,
            .refusals = self.refusals - base.refusals,
            .reaps = self.reaps - base.reaps,
        };
    }
};

/// What can stand between a whole-block fill and the warm serve of its
/// block: the hot tier's evictions and refused puts, the warm ladder's
/// contended probes.
const WarmTier = struct {
    evictions: u64,
    refused_puts: u64,
    contended: u64,

    fn read(rig: *Rig) !WarmTier {
        const text = try rig.stats();
        defer rig.gpa.free(text);
        return .{
            .evictions = numberAfter(text, "\"hot_block_evictions\":") orelse return error.StatMissing,
            .refused_puts = numberAfter(text, "\"hot_block_put_refusals\":") orelse return error.StatMissing,
            .contended = numberAfter(text, "\"ipc_fast_path_lock_demotions\":") orelse return error.StatMissing,
        };
    }

    fn since(self: WarmTier, base: WarmTier) WarmTier {
        return .{ .evictions = self.evictions - base.evictions, .refused_puts = self.refused_puts - base.refused_puts, .contended = self.contended - base.contended };
    }
};

/// What a pwrite's placement leaves once its payloads dropped (they drop
/// after the acks): every assembly it opened is settled — adopted by a
/// merge or reaped unadopted — and none stays charged. Returns the deltas.
fn expectPlacementSettled(rig: *Rig, base: Placement, what: []const u8) !Placement {
    try rig.awaitStat("ipc_placed_assembly_bytes", 0);
    const d = (try Placement.read(rig)).since(base);
    std.debug.print("smoke[interception]: {s} placed {d}/4 (fallbacks {d}), adoptions {d}, refusals {d}, reaps {d}\n", .{ what, d.severs, d.fallbacks, d.adoptions, d.refusals, d.reaps });
    if (d.severs != 0) try testing.expect(d.adoptions + d.reaps >= 1);
    return d;
}

fn served(o: client.Outcome) !usize {
    return switch (o) {
        .served => |n| n,
        .errno => |e| {
            std.debug.print("\n--- ring op answered errno {t} ---\n", .{e});
            return error.TestRingErrno;
        },
        .fallthrough => error.TestRingFallthrough,
    };
}

test "smoke: THE INTERCEPTION HOST — format → mount -o interception → the il0 xattr → a session in-process over the ONE client library → bind → ring pwrite 4 MiB (sever-at-dequeue, the write cache's one record) read back through the kernel path byte-exact → the second block's pieces pool while the record promotes (the file striped from here) → a third block's 4 MiB stream of 1 MiB pieces places, its first merge adopting the assembly (ONE assembly, ONE merge when the four drain in one pass; the bytes whatever the passes did) → a cold read then a warm read on the sync fast path (arena_copy_bytes ≡ bytes), every direct read handed to the atime law → a W1-shaped 4 KiB overwrite serves INLINE on the dd lane (patch_edge_rmw_reads 0) → 8 no-wait tickets complete in offset order → the fd screen's refusals → kernel fsync → umount with the live session tears down cleanly (the client poisoned, no mount left)" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.formatSet();
    // The write cache's one record (below) needs a supervisor to hold it.
    rig.supervise = true;
    try rig.spawnMount();
    _ = try rig.awaitLog("ready: ", 0, ready_wait_ns);
    _ = try rig.awaitLog("ipc: interception armed", 0, ready_wait_ns);

    // The mount's own flags: interception on, write-through forced.
    {
        const text = try rig.stats();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "\"interception\":true") != null);
        try testing.expect(std.mem.indexOf(u8, text, "\"writeback\":false") != null);
    }
    // The credential + the bound file: a regular file on the mount (closed
    // by step 8 before the orderly umount — an open fd is a holder).
    const fd = try rig.openOnMount("ring.dat", .{ .ACCMODE = .RDWR, .CREAT = true, .CLOEXEC = true });
    var fd_open = true;
    defer if (fd_open) {
        _ = linux.close(fd);
    };
    const blob = try rig.il0Blob();
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    try env.put("SQUEEZEFS_TEST_IPC_ALLOW_DIRTY", "1");
    try env.put("SQUEEZEFS_TEST_IL_OP_TIMEOUT_MS", "20000");
    var refusal: ?client.Refusal = null;
    var s = client.Session.establish(gpa, &blob, fd, .{ .build_commit = blob.buildCommit(), .mount_uid = linux.geteuid(), .env = &env }, &refusal) catch |e| {
        std.debug.print("\n--- establish failed: {s} (refusal {any}) ---\n", .{ @errorName(e), refusal });
        return e;
    };
    defer s.teardown();
    try testing.expectEqual(@as(u64, 1), try rig.stat("ipc_sessions_active"));
    try testing.expect((try rig.stat("ipc_service_threads")) >= 1);
    const b = try s.bind(fd, .mount);
    try testing.expect(b.rights.read and b.rights.write);

    // 1. Block 0 through the ring: 4 flights of 1 MiB under one doorbell,
    //    severed at dequeue (the file is inline-born: the pooled path).
    const block0 = try gpa.alloc(u8, BLOCK);
    defer gpa.free(block0);
    pattern(block0, 0);
    try testing.expectEqual(BLOCK, try served(s.pwrite(b.id, block0, 0)));
    try testing.expectEqual(@as(u64, 4), try rig.stat("ipc_ops_write"));
    try testing.expectEqual(@as(u64, 4), try rig.stat("ipc_severs"));
    // Read back through the KERNEL path (a second fd, buffered; closed by
    // step 8 with `fd` — every fd on the mount is a holder).
    const kfd = try rig.openOnMount("ring.dat", .{ .ACCMODE = .RDONLY, .CLOEXEC = true });
    var kfd_open = true;
    defer if (kfd_open) {
        _ = linux.close(kfd);
    };
    const back = try gpa.alloc(u8, BLOCK);
    defer gpa.free(back);
    try testing.expectEqual(BLOCK, linux.pread(kfd, back.ptr, back.len, 0));
    try expectPattern(back, 0);

    // 2. Block 1 as ONE 4 MiB pwrite (4 flights of 1 MiB under one
    //    doorbell). Under the write cache (charter §6.1 "Staged") block 0 is
    //    ONE 4 MiB record in the ring: a chunk severed while the file is
    //    staged severs POOLED (a resident striped layout is placement's
    //    screen); the first handoff's extending write promotes the record —
    //    the file is striped from here — and a chunk severed after that may
    //    place. Which chunks place, and whether a placed one's merge finds
    //    the block's entry (the assembly reaps) or adopts, follows the drain
    //    passes and the lanes: judged are the bytes, one sever per chunk and
    //    every assembly settled.
    const block1 = try gpa.alloc(u8, BLOCK);
    defer gpa.free(block1);
    pattern(block1, BLOCK);
    const before1 = try Placement.read(rig);
    try testing.expectEqual(BLOCK, try served(s.pwrite(b.id, block1, BLOCK)));
    try testing.expectEqual(@as(u64, 8), try rig.stat("ipc_ops_write"));
    try testing.expectEqual(@as(u64, 8), try rig.stat("ipc_severs"));
    try testing.expectEqual(BLOCK, linux.pread(kfd, back.ptr, back.len, @intCast(BLOCK)));
    try expectPattern(back, BLOCK);
    _ = try expectPlacementSettled(rig, before1, "block 1");
    //    Block 2 as ONE 4 MiB pwrite: the file is striped and block 2 has
    //    no entry, so its first chunk takes the placed arm (it places, or
    //    falls back counted) and the first merge of the block meets a
    //    placed payload: it adopts the assembly with its LANDED runs as
    //    coverage, or is refused under the Dekker. The client pushes each
    //    flight after its copy, so a pass may drain a prefix of the four
    //    (a park's rescan, a client preempted between flights): the
    //    prefix's flush adopts it and the later chunks find the entry and
    //    sever pooled (`il_daemon.md` §3.2: an entry-present block never
    //    adopts). Four placed with no refusal is ONE assembly adopted full
    //    by ONE merge, its siblings riding it.
    pattern(block1, 2 * BLOCK);
    const before2 = try Placement.read(rig);
    try testing.expectEqual(BLOCK, try served(s.pwrite(b.id, block1, 2 * BLOCK)));
    try testing.expectEqual(@as(u64, 12), try rig.stat("ipc_ops_write"));
    try testing.expectEqual(@as(u64, 12), try rig.stat("ipc_severs"));
    const placed2 = try expectPlacementSettled(rig, before2, "block 2");
    try testing.expect(placed2.severs + placed2.fallbacks >= 1);
    if (placed2.severs != 0 and placed2.fallbacks == 0) try testing.expect(placed2.adoptions + placed2.refusals >= 1);
    if (placed2.severs == 4 and placed2.refusals == 0) {
        try testing.expectEqual(@as(u64, 1), placed2.adoptions);
        try testing.expectEqual(@as(u64, 0), placed2.reaps);
    }
    try testing.expectEqual(BLOCK, linux.pread(kfd, back.ptr, back.len, @intCast(2 * BLOCK)));
    // The kernel-path read-backs ran contiguous: a CLASSIFIED stream, which
    // vetoes ranged windows file-wide while fresh (`prefetch.STALE_NS`).
    const stream_stale_at = clock.monotonicNs() + prefetch.STALE_NS;
    try expectPattern(back, 2 * BLOCK);
    // Every parked block uploads (the write-through) before the W1 row.
    try rig.awaitStat("parked_buffers", 0);
    // Step 3 judges the COLD ladder, so the stream must be stale first: on a
    // fast host (kamrui, x86_64 NVMe) the upload settles inside the window and
    // the unaligned read below was a 4 MiB whole-block fill, its block hot
    // before the "second touch".
    while (clock.monotonicNs() < stream_stale_at) sleepMs(50);

    // 3. Reads on the sync fast path. A cold ALIGNED ring read is the
    //    direct-drive READ: the leg the ladder resolved DMAs straight into
    //    the arena on the service thread's own shard (zero daemon copies,
    //    no lane handoff, never published — demoted, not warmed, no ghost
    //    touch); a cold UNALIGNED read is a RANGED window too (decision
    //    0003 §K2: every strictly sub-block random read — the 256 KiB
    //    ceiling that sent it to the whole-block walk was the read-LRU's
    //    population bound borrowed as a DMA policy) and the ghost records
    //    the touch; its SECOND touch escalates to the ONE whole-block fill
    //    that warms the hot tier; the next read of that block serves WARM on
    //    the service thread with exactly its bytes on the arena copy ledger.
    const rbuf = try gpa.alloc(u8, 512 * 1024 + 100);
    defer gpa.free(rbuf);
    // 3a. THE READ CACHE on the ring arm (charter §6.3): a cold ALIGNED
    //     4 KiB ring read is the direct-drive READ whose DMA lands in the
    //     session arena — memory this process owns — so its CQE hook
    //     DEPOSITS the window (`direct_leg_deposits`); the second read of
    //     the window is a fast-path serve from the grains (no dd read, no
    //     lane) with exactly its bytes on the arena copy ledger. The
    //     grain step precedes the ghost steps below: the escalation warms
    //     the whole block into the hot tier, which outranks the grains. A
    //     deposit is declined under budget Red (the tier's law), so the
    //     hit arm is judged only below it.
    {
        const g0 = try rig.stat("read_cache_grain_hits");
        const dep0 = try rig.stat("direct_leg_deposits");
        const dd0 = try rig.stat("ipc_dd_read_serves");
        const fp0 = try rig.stat("ipc_fast_path_serves");
        const gc0 = try rig.stat("ipc_read_arena_copy_bytes");
        try testing.expectEqual(@as(usize, 4096), try served(s.pread(b.id, rbuf[0..4096], 2 * MiB)));
        try expectPattern(rbuf[0..4096], 2 * MiB);
        try testing.expectEqual(dd0 + 1, try rig.stat("ipc_dd_read_serves"));
        const level = try rig.stat("mem_budget_level");
        if (level != 2) {
            try testing.expectEqual(dep0 + 1, try rig.stat("direct_leg_deposits"));
            try testing.expectEqual(@as(usize, 4096), try served(s.pread(b.id, rbuf[0..4096], 2 * MiB)));
            try expectPattern(rbuf[0..4096], 2 * MiB);
            try testing.expectEqual(g0 + 1, try rig.stat("read_cache_grain_hits"));
            try testing.expectEqual(fp0 + 1, try rig.stat("ipc_fast_path_serves"));
            try testing.expectEqual(gc0 + 4096, try rig.stat("ipc_read_arena_copy_bytes"));
            try testing.expectEqual(dd0 + 1, try rig.stat("ipc_dd_read_serves"));
        } else std.debug.print("smoke[interception]: budget Red at the grain step — the deposit and the hit are not judged on this run\n", .{});
    }
    const demotes0 = try rig.stat("ipc_fast_path_miss_demotions");
    const dd_reads0 = try rig.stat("ipc_dd_read_serves");
    const dma0 = try rig.stat("ipc_read_arena_dma_bytes");
    const handoffs0 = try rig.stat("ipc_read_handoffs");
    const esc0 = try rig.stat("ranged_read_ghost_escalations");
    try testing.expectEqual(@as(usize, 512 * 1024), try served(s.pread(b.id, rbuf[0 .. 512 * 1024], 1 * MiB)));
    try expectPattern(rbuf[0 .. 512 * 1024], 1 * MiB);
    try testing.expectEqual(dd_reads0 + 1, try rig.stat("ipc_dd_read_serves"));
    try testing.expectEqual(dma0 + 512 * 1024, try rig.stat("ipc_read_arena_dma_bytes"));
    try testing.expectEqual(handoffs0, try rig.stat("ipc_read_handoffs"));
    try testing.expectEqual(@as(u64, 0), try rig.stat("ipc_dd_read_fallbacks_post"));
    try testing.expectEqual(rbuf.len, try served(s.pread(b.id, rbuf, 1 * MiB + 100)));
    try expectPattern(rbuf, 1 * MiB + 100);
    try testing.expectEqual(demotes0 + 2, try rig.stat("ipc_fast_path_miss_demotions"));
    try testing.expectEqual(esc0, try rig.stat("ranged_read_ghost_escalations"));
    const level_touch = try rig.stat("mem_budget_level");
    const tier0 = try WarmTier.read(rig);
    try testing.expectEqual(rbuf.len, try served(s.pread(b.id, rbuf, 1 * MiB + 100)));
    try expectPattern(rbuf, 1 * MiB + 100);
    try testing.expectEqual(demotes0 + 3, try rig.stat("ipc_fast_path_miss_demotions"));
    // The ghost's second touch escalates unless the budget is Red (the
    // admission's law: `rangedEscalation` refuses under Red) — a 512 KiB
    // window is outside the read cache's grain class, so no serve stands
    // between the two touches; under Red the warm-serve arm is not judged.
    // The escalated block serves WARM on the next read unless a lawful arm
    // stood between: the budget off Green (an insert at Yellow is net-zero
    // growth — the empty tier sheds the block it just took), a tier
    // eviction or a refused put, or a probe that met a held guard (a
    // contended probe demotes, never waits) — the read then rides the
    // handoff with the same bytes.
    const level_esc = try rig.stat("mem_budget_level");
    if (level_touch != 2 and level_esc != 2) {
        try testing.expectEqual(esc0 + 1, try rig.stat("ranged_read_ghost_escalations"));
        const serves0 = try rig.stat("ipc_fast_path_serves");
        const copy0 = try rig.stat("ipc_read_arena_copy_bytes");
        const ops_read0 = try rig.stat("ipc_ops_read");
        try testing.expectEqual(@as(usize, 512 * 1024), try served(s.pread(b.id, rbuf[0 .. 512 * 1024], 1 * MiB)));
        try expectPattern(rbuf[0 .. 512 * 1024], 1 * MiB);
        try testing.expectEqual(ops_read0 + 1, try rig.stat("ipc_ops_read"));
        const level_warm = try rig.stat("mem_budget_level");
        const t = (try WarmTier.read(rig)).since(tier0);
        if (level_touch == 0 and level_esc == 0 and level_warm == 0 and t.evictions == 0 and t.refused_puts == 0 and t.contended == 0) {
            try testing.expectEqual(serves0 + 1, try rig.stat("ipc_fast_path_serves"));
            try testing.expectEqual(copy0 + 512 * 1024, try rig.stat("ipc_read_arena_copy_bytes"));
        } else std.debug.print("smoke[interception]: the warm serve is not judged on this run — budget levels {d}/{d}/{d}, tier evictions {d}, refused puts {d}, contended probes {d}\n", .{ level_touch, level_esc, level_warm, t.evictions, t.refused_puts, t.contended });
    } else std.debug.print("smoke[interception]: budget Red at the ghost step — the escalation and the warm serve are not judged on this run\n", .{});
    try testing.expect((try rig.stat("ipc_read_demote_bytes")) >= 2 * 512 * 1024);
    // The engagement identity closes.
    {
        const text = try rig.stats();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "\"closure\":{\"reads\":true,\"writes\":true,\"dd\":true}") != null);
    }
    // The atime law on the dd lane (charter §4 "Access time"): every read it
    // served was handed to the law before its ACK — judged at the lane's
    // counter (a parked stamp absorbs the next read's, so neither
    // `fuse_atime_touches` nor `stat` is exact here).
    try testing.expect((try rig.stat("ipc_dd_read_serves")) >= 2);
    try testing.expectEqual(try rig.stat("ipc_dd_read_serves"), try rig.stat("ipc_dd_read_atime_notes"));

    // 4. The W1 shape INLINE on the dd lane: 4 KiB aligned into block 0,
    //    non-extending, not stream-adjacent.
    const inline0 = try rig.stat("ipc_dd_inline_serves");
    const rmw0 = try rig.stat("patch_edge_rmw_reads");
    var page: [4096]u8 = undefined;
    pattern(&page, 0xA000_0000);
    try testing.expectEqual(page.len, try served(s.pwrite(b.id, &page, 8192)));
    try testing.expectEqual(inline0 + 1, try rig.stat("ipc_dd_inline_serves"));
    try testing.expectEqual(rmw0, try rig.stat("patch_edge_rmw_reads"));
    try testing.expectEqual(page.len, linux.pread(kfd, back.ptr, page.len, 8192));
    try expectPattern(back[0..page.len], 0xA000_0000);
    try testing.expectEqual(page.len, linux.pread(kfd, back.ptr, page.len, 4096));
    try expectPattern(back[0..page.len], 4096);

    // 5. A libaio-shaped batch: 8 no-wait tickets (one ring op per iocb),
    //    reaped in offset order — every result its length.
    //    (above the page the W1 row overwrote).
    const tick_base: u64 = 256 * 1024;
    var tickets: [8]client.Ticket = undefined;
    for (&tickets, 0..) |*t, i| t.* = s.submitNoWait(.read, b.id, tick_base + @as(u64, i) * 32 * 1024, 32 * 1024, null) orelse return error.TestNoTicket;
    var got: [32 * 1024]u8 = undefined;
    for (tickets, 0..) |t, i| {
        const deadline = clock.monotonicNs() + 5 * std.time.ns_per_s;
        while (true) {
            if (s.pollTicket(t, &got)) |r| {
                try testing.expectEqual(@as(i64, 32 * 1024), r);
                break;
            }
            if (clock.monotonicNs() > deadline) return error.TestTicketTimeout;
            sleepMs(1);
        }
        try expectPattern(&got, tick_base + @as(u64, i) * 32 * 1024);
    }
    // The libaio harness's shape (`shim/smoke.zig`): 8 × 4 KiB WRITES at
    // 0..28 KiB into a fresh, empty file in ONE batch — small extending
    // writes to one block, severed and merged concurrently; the second one
    // promotes the inline file to striped (the own-block proof covers the
    // slot: `plane.ownBlock`) — every result its length, the bytes whole
    // through the kernel. The same shape through the KERNEL path first:
    // the promotion is the data plane's, whatever the transport.
    {
        const kfd2 = try rig.openOnMount("kaio.dat", .{ .ACCMODE = .RDWR, .CREAT = true, .TRUNC = true, .CLOEXEC = true });
        defer _ = linux.close(kfd2);
        var wpage: [4096]u8 = undefined;
        for (0..8) |i| {
            @memset(&wpage, 'A' + @as(u8, @intCast(i)));
            try testing.expectEqual(wpage.len, linux.pwrite(kfd2, &wpage, wpage.len, @intCast(i * 4096)));
        }
        var kback: [8 * 4096]u8 = undefined;
        try testing.expectEqual(kback.len, linux.pread(kfd2, &kback, kback.len, 0));
        for (0..8) |i| for (kback[i * 4096 .. (i + 1) * 4096]) |c| try testing.expectEqual('A' + @as(u8, @intCast(i)), c);

        const afd = try rig.openOnMount("aio.dat", .{ .ACCMODE = .RDWR, .CREAT = true, .TRUNC = true, .CLOEXEC = true });
        defer _ = linux.close(afd);
        const ab = try s.bind(afd, .mount);
        defer s.unbind(ab.id);
        var wt: [8]client.Ticket = undefined;
        for (&wt, 0..) |*t, i| {
            @memset(&wpage, 'a' + @as(u8, @intCast(i)));
            t.* = s.submitNoWait(.write, ab.id, @as(u64, i) * 4096, 4096, &wpage) orelse return error.TestNoTicket;
        }
        var results: [8]i64 = undefined;
        for (wt, 0..) |t, i| {
            const deadline = clock.monotonicNs() + 5 * std.time.ns_per_s;
            while (true) {
                if (s.pollTicket(t, null)) |r| {
                    results[i] = r;
                    break;
                }
                if (clock.monotonicNs() > deadline) return error.TestTicketTimeout;
                sleepMs(1);
            }
        }
        for (results) |r| try testing.expectEqual(@as(i64, 4096), r);
        var aback: [8 * 4096]u8 = undefined;
        try testing.expectEqual(aback.len, linux.pread(afd, &aback, aback.len, 0));
        for (0..8) |i| for (aback[i * 4096 .. (i + 1) * 4096]) |c| try testing.expectEqual('a' + @as(u8, @intCast(i)), c);
    }

    // 6. The fd screen through the real session: a read-only binding's
    //    write is EBADF; O_PATH, a directory, O_APPEND ⇒ flags; a memfd
    //    (a foreign device) ⇒ mount. Every fd closes with the block.
    {
        const ro = try rig.openOnMount("ring.dat", .{ .ACCMODE = .RDONLY, .CLOEXEC = true });
        defer _ = linux.close(ro);
        const rob = try s.bind(ro, .mount);
        defer s.unbind(rob.id);
        try testing.expect(rob.rights.read and !rob.rights.write);
        switch (s.pwrite(rob.id, &page, 0)) {
            .errno => |e| try testing.expectEqual(linux.E.BADF, e),
            else => return error.TestExpectedEbadf,
        }
        const op = try rig.openOnMount("ring.dat", .{ .ACCMODE = .RDONLY, .PATH = true, .CLOEXEC = true });
        defer _ = linux.close(op);
        try testing.expectError(error.Refused, s.bind(op, .mount));
        try testing.expectEqual(wire.RefuseClass.flags, s.last_refusal.?);
        const dfd = try rig.openOnMount("", .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .CLOEXEC = true });
        defer _ = linux.close(dfd);
        try testing.expectError(error.Refused, s.bind(dfd, .mount));
        try testing.expectEqual(wire.RefuseClass.flags, s.last_refusal.?);
        const ap = try rig.openOnMount("ring.dat", .{ .ACCMODE = .WRONLY, .APPEND = true, .CLOEXEC = true });
        defer _ = linux.close(ap);
        try testing.expectError(error.Refused, s.bind(ap, .mount));
        try testing.expectEqual(wire.RefuseClass.flags, s.last_refusal.?);
        const mfd: linux.fd_t = @intCast(linux.memfd_create("sqz-foreign", linux.MFD.CLOEXEC));
        defer _ = linux.close(mfd);
        try testing.expectError(error.Refused, s.bind(mfd, .mount));
        try testing.expectEqual(wire.RefuseClass.mount, s.last_refusal.?);
    }

    // 7. fsync through the kernel after the ring writes (the §5.6.3 order).
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.fsync(fd)));
    {
        const text = try rig.stats();
        defer gpa.free(text);
        try testing.expectEqual(@as(?u64, 0), numberAfter(text, "\"ipc_sessions_poisoned\":"));
        // Exactly the EBADF row above: the wrong-direction tripwire.
        try testing.expectEqual(@as(?u64, 1), numberAfter(text, "\"ipc_descriptor_rejects\":"));
        try testing.expect((numberAfter(text, "\"ipc_inval_notifies\":") orelse 0) >= 1);
        try testing.expectEqual(numberAfter(text, "\"ipc_dd_read_serves\":"), numberAfter(text, "\"ipc_dd_read_atime_notes\":"));
    }

    // 8. `umount` while THIS process still holds `ring.dat` open: refused
    //    by name (decision 0003 §S — an open fd is a holder; the verb names
    //    the pid and the fd), the mount serving on. Then the fd closed and
    //    `umount` with the LIVE session (the socket, its bindings): the
    //    host poisons it (the client sees the generation bump), the daemon
    //    exits 0, no mount is left.
    {
        const r = try std.process.run(gpa, io, .{ .argv = &.{ rig.bin, "umount", rig.mp }, .environ_map = &rig.env });
        defer gpa.free(r.stdout);
        defer gpa.free(r.stderr);
        try testing.expectEqual(std.process.Child.Term{ .exited = 1 }, r.term);
        var want: [64]u8 = undefined;
        try testing.expect(std.mem.indexOf(u8, r.stderr, try std.fmt.bufPrint(&want, "pid {d} (", .{linux.getpid()})) != null);
        try testing.expect(std.mem.indexOf(u8, r.stderr, try std.fmt.bufPrint(&want, ") fd {d}", .{fd})) != null);
        try testing.expectEqual(@as(usize, 1), try fuseMountsOf(gpa, io, rig.mp));
    }
    kfd_open = false;
    _ = linux.close(kfd);
    fd_open = false;
    _ = linux.close(fd);
    try rig.umount();
    const term = try rig.waitChild();
    try testing.expectEqual(std.process.Child.Term{ .exited = 0 }, term);
    try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, rig.mp));
    try testing.expect(s.poisoned());
    try testing.expect(s.pread(b.id, rbuf, 0) == .fallthrough);
    {
        const text = try rig.readLog();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "torn down: host shutdown") != null);
        // The one tripwire line is the EBADF row's; nothing else fired.
        try testing.expectEqual(@as(usize, 1), std.mem.count(u8, text, "INVARIANT TRIPWIRE"));
        try testing.expect(std.mem.indexOf(u8, text, "write on a read-only binding") != null);
        // The daemon's allocator audits itself at a graceful exit: a live
        // session torn down at disarm must leave nothing behind.
        try testing.expect(std.mem.indexOf(u8, text, "leaked") == null);
    }
    rig.passed = true;
}

test "smoke: THE INTERCEPTION HOST, death — kill -9 the daemon under a parked ring op ⇒ the heartbeat stops and the socket dies ⇒ the client poisons inside the ladder (≈ 1 s), far inside the 20 s seam deadline; the mount is detached" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.formatSet();
    try rig.spawnMount();
    _ = try rig.awaitLog("ipc: interception armed", 0, ready_wait_ns);
    const fd = try rig.openOnMount("dead.dat", .{ .ACCMODE = .RDWR, .CREAT = true, .CLOEXEC = true });
    defer _ = linux.close(fd);
    const blob = try rig.il0Blob();
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    try env.put("SQUEEZEFS_TEST_IPC_ALLOW_DIRTY", "1");
    try env.put("SQUEEZEFS_TEST_IL_OP_TIMEOUT_MS", "20000");
    var refusal: ?client.Refusal = null;
    var s = try client.Session.establish(gpa, &blob, fd, .{ .build_commit = blob.buildCommit(), .mount_uid = linux.geteuid(), .env = &env }, &refusal);
    defer s.teardown();
    const b = try s.bind(fd, .mount);
    var buf: [4096]u8 = @splat(7);
    try testing.expectEqual(buf.len, try served(s.pwrite(b.id, &buf, 0)));
    // The daemon dies mid-life; the mount is aborted by the kernel.
    var c = rig.child.?;
    rig.child = null;
    _ = linux.kill(rig.child_pid, .KILL);
    _ = c.wait(io) catch {};
    const t0 = clock.monotonicNs();
    try testing.expect(s.pread(b.id, &buf, 0) == .fallthrough);
    const elapsed = clock.monotonicNs() - t0;
    try testing.expect(elapsed < 5 * std.time.ns_per_s);
    try testing.expect(s.poisoned());
    _ = linux.umount2(rig.mp.ptr, linux.MNT.DETACH);
    try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, rig.mp));
    rig.passed = true;
}

test "smoke: THE DIRECT ARM — mount -o interception=direct → a root session is granted the arm (the lease page sealed and mapped, the data devices open) → a striped file's lease → a direct pread lands the device's bytes byte-exact from the client's own thread (ipc_ops_read flat, il_direct_ops +1) → a truncate through the kernel between the lease and the read moves the stripe's epoch: the recheck refuses, the lease is re-asked, the read answers the new EOF → a kernel write admitted on the inode invalidates the lease (re-asked, the new bytes read) → an inline-born file's lease is refused (kind) and the ring serves it → THE WRITE ARM: a direct write of a resident page is one pwrite on the device under a write lease (byte-exact through O_DIRECT, ipc_ops_write flat, the same lease serves the read), an extending write is the ring's, fsync acks the note and syncs the device, a write held in flight makes a kernel truncate WAIT at the recall and the next write finds the lease recalled → kill -9 the daemon: the next direct read sees the dead heartbeat and falls through (never a serve under a lease nobody can invalidate)" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    rig.mount_opts = "interception=direct";
    try rig.formatSet();
    try rig.spawnMount();
    _ = try rig.awaitLog("ready: ", 0, ready_wait_ns);
    _ = try rig.awaitLog("ipc: interception armed", 0, ready_wait_ns);
    const fd = try rig.openOnMount("direct.dat", .{ .ACCMODE = .RDWR, .CREAT = true, .CLOEXEC = true });
    defer _ = linux.close(fd);
    const blob = try rig.il0Blob();
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    try env.put("SQUEEZEFS_TEST_IPC_ALLOW_DIRTY", "1");
    try env.put("SQUEEZEFS_TEST_IL_OP_TIMEOUT_MS", "20000");
    var refusal: ?client.Refusal = null;
    var s = client.Session.establish(gpa, &blob, fd, .{ .build_commit = blob.buildCommit(), .mount_uid = linux.geteuid(), .env = &env }, &refusal) catch |e| {
        std.debug.print("\n--- establish failed: {s} (refusal {any}) ---\n", .{ @errorName(e), refusal });
        return e;
    };
    defer s.teardown();
    try testing.expectEqual(client.Session.DirectState.on, s.direct_state);
    try testing.expect(s.direct != null);
    try testing.expect(s.features & wire.FEATURE_DIRECT != 0);
    try testing.expectEqual(@as(u64, 1), try rig.stat("ipc_direct_sessions"));
    try testing.expectEqual(@as(u64, 0), try rig.stat("ipc_direct_refused_access"));
    const b = try s.bind(fd, .mount);

    // Two blocks through the ring (the file is striped), the write-through
    // drained: the lease needs a resident map with no custody.
    const block = try gpa.alloc(u8, BLOCK);
    defer gpa.free(block);
    pattern(block, 0);
    try testing.expectEqual(BLOCK, try served(s.pwrite(b.id, block, 0)));
    pattern(block, BLOCK);
    try testing.expectEqual(BLOCK, try served(s.pwrite(b.id, block, BLOCK)));
    // The write asks above were refused and PARKED the binding for the
    // client's refusal backoff (`REFUSE_BACKOFF_NS`: one ask per backoff,
    // never one per op): a read inside the park is the ring's without an
    // ask. Under the write cache the two 4 MiB writes land in RAM fast
    // enough that the read below fell inside it — wait the park out (the
    // backoff is the client's law, not a synchronization).
    const park_until = clock.monotonicNs() + client.direct_client.REFUSE_BACKOFF_NS;
    try rig.awaitStat("parked_buffers", 0);
    const kfd = try rig.openOnMount("direct.dat", .{ .ACCMODE = .RDWR, .CLOEXEC = true });
    defer _ = linux.close(kfd);
    if (linux.errno(linux.fsync(kfd)) != .SUCCESS) return error.TestFsync;
    while (clock.monotonicNs() < park_until) sleepMs(20);

    // 1. The direct read: the device's bytes, no ring op.
    const rbuf = try gpa.alignedAlloc(u8, .fromByteUnits(4096), 64 * 1024);
    defer gpa.free(rbuf);
    const ops0 = try rig.stat("ipc_ops_read");
    const page = s.statsPage();
    // (The 4 MiB ring writes above asked and were refused — an inline-born
    // file, a staged one, an extending write: whichever asks the park let
    // through — so the counts below are relative.)
    const asks0 = page.il_lease_asks.load(.monotonic);
    const granted0 = try rig.stat("ipc_leases_granted");
    try testing.expectEqual(@as(usize, 64 * 1024), try served(s.pread(b.id, rbuf, 1 * MiB)));
    try expectPattern(rbuf, 1 * MiB);
    try testing.expectEqual(ops0, try rig.stat("ipc_ops_read"));
    try testing.expectEqual(@as(u64, 1), page.il_direct_ops.load(.monotonic));
    try testing.expectEqual(asks0 + 1, page.il_lease_asks.load(.monotonic));
    try testing.expectEqual(granted0 + 1, try rig.stat("ipc_leases_granted"));
    // Block 1 under the same lease, then a read past EOF.
    try testing.expectEqual(@as(usize, 4096), try served(s.pread(b.id, rbuf[0..4096], BLOCK + 8192)));
    try expectPattern(rbuf[0..4096], BLOCK + 8192);
    try testing.expectEqual(@as(usize, 0), try served(s.pread(b.id, rbuf[0..4096], 2 * BLOCK)));
    try testing.expectEqual(asks0 + 1, page.il_lease_asks.load(.monotonic));

    // 2. The window: a truncate lands between the client's lease and its
    //    read. The lease (8 MiB) still names block 0's bytes at 1 MiB; the
    //    device still holds them; the recheck sees the moved epoch, the
    //    lease is asked again and the read answers the new EOF.
    if (linux.errno(linux.ftruncate(kfd, 4096)) != .SUCCESS) return error.TestTruncate;
    try testing.expectEqual(@as(usize, 0), try served(s.pread(b.id, rbuf, 1 * MiB)));
    try testing.expectEqual(@as(u64, 1), page.il_direct_rechecks_failed.load(.monotonic));
    try testing.expectEqual(asks0 + 2, page.il_lease_asks.load(.monotonic));
    // (The re-asked lease may be refused while the clipped block's custody
    // drains — the ring then answers the EOF; either way the stale lease
    // served nothing.)
    try testing.expect((try rig.stat("ipc_ops_read")) <= ops0 + 1);

    // 3. A kernel write admitted on the inode invalidates the lease: the
    //    next direct read re-asks and reads the new bytes.
    var kpage: [4096]u8 = undefined;
    pattern(&kpage, 0x7000_0000);
    try testing.expectEqual(kpage.len, linux.pwrite(kfd, &kpage, kpage.len, 0));
    if (linux.errno(linux.fsync(kfd)) != .SUCCESS) return error.TestFsync;
    try rig.awaitStat("parked_buffers", 0);
    var asks_before = page.il_lease_asks.load(.monotonic);
    const deadline = clock.monotonicNs() + 10 * std.time.ns_per_s;
    while (true) {
        // The custody of the write drains on the write-through cadence; a
        // refused lease (custody) backs off and the ring serves meanwhile.
        const n = try served(s.pread(b.id, rbuf[0..4096], 0));
        try testing.expectEqual(@as(usize, 4096), n);
        try expectPattern(rbuf[0..4096], 0x7000_0000);
        if (page.il_direct_ops.load(.monotonic) >= 3) break;
        if (clock.monotonicNs() > deadline) return error.TestDirectNotResumed;
        sleepMs(client.direct_client.REFUSE_BACKOFF_NS / std.time.ns_per_ms + 10);
    }
    try testing.expect(page.il_lease_asks.load(.monotonic) > asks_before);
    asks_before = page.il_lease_asks.load(.monotonic);

    // 4. An inline-born small file: the lease refuses (kind), the ring serves.
    {
        const sfd = try rig.openOnMount("small.dat", .{ .ACCMODE = .RDWR, .CREAT = true, .TRUNC = true, .CLOEXEC = true });
        defer _ = linux.close(sfd);
        var small: [100]u8 = undefined;
        pattern(&small, 0x9000);
        try testing.expectEqual(small.len, linux.pwrite(sfd, &small, small.len, 0));
        const sb = try s.bind(sfd, .mount);
        defer s.unbind(sb.id);
        const kind0 = try rig.stat("ipc_leases_refused_kind");
        const ring0 = try rig.stat("ipc_ops_read");
        try testing.expectEqual(@as(usize, 100), try served(s.pread(sb.id, rbuf[0..4096], 0)));
        try expectPattern(rbuf[0..100], 0x9000);
        // The backoff: the second read, inside the window, asks nothing
        // and rides the ring (the counts read back to back: a `.stats`
        // read of this debug build can take a good part of the window).
        const asks_after_first = page.il_lease_asks.load(.monotonic);
        try testing.expectEqual(@as(usize, 100), try served(s.pread(sb.id, rbuf[0..4096], 0)));
        try testing.expectEqual(asks_after_first, page.il_lease_asks.load(.monotonic));
        try testing.expectEqual(kind0 + 1, try rig.stat("ipc_leases_refused_kind"));
        try testing.expectEqual(ring0 + 2, try rig.stat("ipc_ops_read"));
    }

    // 4b. THE DIRECT WRITE ARM (decision 0003 §AY). The session holds the
    //     write arm (root writes the image files); the file is one resident
    //     block of 4 KiB: a direct write of page 0 is one pwrite on the
    //     device under a WRITE lease, byte-exact through an O_DIRECT kernel
    //     fd, with ipc_ops_write flat.
    try testing.expect(s.features & wire.FEATURE_DIRECT_WRITE != 0);
    try testing.expectEqual(@as(u64, 1), try rig.stat("ipc_direct_write_sessions"));
    const dfd = try rig.openOnMount("direct.dat", .{ .ACCMODE = .RDONLY, .DIRECT = true, .CLOEXEC = true });
    defer _ = linux.close(dfd);
    const wops0 = try rig.stat("ipc_ops_write");
    const wleases0 = try rig.stat("ipc_write_leases_granted");
    const wpage = try gpa.alignedAlloc(u8, .fromByteUnits(4096), 4096);
    defer gpa.free(wpage);
    pattern(wpage, 0xD1EC_0000);
    try testing.expectEqual(@as(usize, 4096), try served(s.pwrite(b.id, wpage, 0)));
    try testing.expectEqual(@as(u64, 1), page.il_direct_writes.load(.monotonic));
    try testing.expectEqual(wops0, try rig.stat("ipc_ops_write"));
    try testing.expectEqual(wleases0 + 1, try rig.stat("ipc_write_leases_granted"));
    try testing.expectEqual(@as(u64, 1), try rig.stat("ipc_write_leases_live"));
    try testing.expectEqual(@as(usize, 4096), linux.pread(dfd, rbuf.ptr, 4096, 0));
    try expectPattern(rbuf[0..4096], 0xD1EC_0000);
    // The same page read direct: the lease is one object for both arms.
    try testing.expectEqual(@as(usize, 4096), try served(s.pread(b.id, rbuf[0..4096], 0)));
    try expectPattern(rbuf[0..4096], 0xD1EC_0000);
    // An extending write is the ring's (size-neutral law); the lease stands.
    pattern(wpage, 0xD1EC_1000);
    try testing.expectEqual(@as(usize, 4096), try served(s.pwrite(b.id, wpage, 4096)));
    try testing.expectEqual(@as(u64, 1), page.il_direct_writes.load(.monotonic));
    try testing.expectEqual(wops0 + 1, try rig.stat("ipc_ops_write"));
    try rig.awaitStat("parked_buffers", 0);

    // 4c. fsync on the direct arm: the pending note sent and acked (the
    //     daemon's dirty mark and KD-11 notify), the device fdatasync'd.
    try testing.expect(s.directFsync(b.id));
    try testing.expect((try rig.stat("ipc_direct_write_notes")) >= 1);
    try testing.expect((try rig.stat("ipc_direct_writes")) >= 1);

    // 4d. THE RECALL: a client write held in flight (the ledger's count,
    //     exactly what a pwrite in progress holds) while a truncate through
    //     the kernel wants the inode: the daemon's exclusive guard recalls
    //     the lease and WAITS; the truncate lands only once the write
    //     exits, and the client's next write finds the lease recalled.
    {
        // The ring write above recalled the lease (every kernel-path
        // admission does) and moved the epoch (its arm left bytes off the
        // device): this write drops the stale lease at the epoch check and
        // a fresh write lease comes with its second attempt.
        pattern(wpage, 0xD1EC_2000);
        const rechecks0 = page.il_direct_rechecks_failed.load(.monotonic);
        try testing.expectEqual(@as(usize, 4096), try served(s.pwrite(b.id, wpage, 0)));
        try testing.expectEqual(@as(u64, 2), page.il_direct_writes.load(.monotonic));
        try testing.expectEqual(rechecks0 + 1, page.il_direct_rechecks_failed.load(.monotonic));
        try testing.expectEqual(@as(u64, 0), page.il_direct_writes_recalled.load(.monotonic));
        const l = s.direct.?.leaseFor(b.id) orelse return error.TestNoLease;
        defer l.drop();
        try testing.expect(l.writable);
        const words = &page.lease_ledger[l.slot];
        try testing.expect(client.direct_client_core.enterWrite(words, l.tag));
        const Truncator = struct {
            fd: linux.fd_t,
            done: *Atomic(bool),
            fn run(self: @This()) void {
                _ = linux.ftruncate(self.fd, 0);
                self.done.store(true, .release);
            }
        };
        var done: Atomic(bool) = .init(false);
        const recalls0 = try rig.stat("ipc_write_lease_recalls");
        const t = try std.Thread.spawn(.{}, Truncator.run, .{Truncator{ .fd = kfd, .done = &done }});
        // The daemon waits on the count: the truncate does not land.
        sleepMs(150);
        try testing.expect(!done.load(.acquire));
        try testing.expectEqual(recalls0 + 1, try rig.stat("ipc_write_lease_recalls"));
        try testing.expectEqual(@as(u64, 0), try rig.stat("ipc_write_leases_forced"));
        // The write exits: the daemon drains and the truncate lands.
        if (client.direct_client_core.exitWrite(words)) client.futex.wakeSharedAll(&words.inflight);
        t.join();
        try testing.expect(done.load(.acquire));
        try testing.expectEqual(@as(u64, 1), try rig.stat("ipc_write_lease_recalls_drained"));
        try testing.expectEqual(@as(u64, 0), try rig.stat("ipc_write_leases_live"));
        // The next write finds the old lease dead — at the epoch check
        // (the truncate's commit moved it) or at entry (recalled) — and
        // re-asks: the file is empty now, so the ring extends it.
        pattern(wpage, 0xD1EC_3000);
        const rechecks1 = page.il_direct_rechecks_failed.load(.monotonic);
        try testing.expectEqual(@as(usize, 4096), try served(s.pwrite(b.id, wpage, 0)));
        try testing.expect(page.il_direct_rechecks_failed.load(.monotonic) + page.il_direct_writes_recalled.load(.monotonic) > rechecks1);
        try testing.expectEqual(@as(u64, 2), page.il_direct_writes.load(.monotonic));
        // The bytes through the arm (the ring write's grow reaches the
        // kernel's size on the KD-11 sideband, not before this read).
        try rig.awaitStat("parked_buffers", 0);
        try testing.expectEqual(@as(usize, 4096), try served(s.pread(b.id, rbuf[0..4096], 0)));
        try expectPattern(rbuf[0..4096], 0xD1EC_3000);
    }

    // 5. Kill -9 the daemon: the lease page's epochs can no longer move,
    //    so the client must not serve under them — the heartbeat ladder
    //    poisons the session and the read falls through.
    const direct_ops = page.il_direct_ops.load(.monotonic);
    var c = rig.child.?;
    rig.child = null;
    _ = linux.kill(rig.child_pid, .KILL);
    _ = c.wait(io) catch {};
    const t0 = clock.monotonicNs();
    while (true) {
        switch (s.pread(b.id, rbuf[0..4096], 0)) {
            .fallthrough => break,
            .served => {},
            .errno => return error.TestRingErrno,
        }
        if (clock.monotonicNs() - t0 > 5 * std.time.ns_per_s) return error.TestNoPoison;
        sleepMs(50);
    }
    try testing.expect(s.poisoned());
    // At most the reads inside the heartbeat's grace served; none after.
    try testing.expect(page.il_direct_ops.load(.monotonic) >= direct_ops);
    _ = linux.umount2(rig.mp.ptr, linux.MNT.DETACH);
    try testing.expectEqual(@as(usize, 0), try fuseMountsOf(gpa, io, rig.mp));
    rig.passed = true;
}
