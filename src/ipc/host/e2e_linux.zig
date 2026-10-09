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
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;

const clock = @import("../../core/clock.zig");
const log = @import("../../core/log.zig");
const wire = @import("../wire.zig");
const client = @import("../client.zig");
const rendezvous = @import("../rendezvous.zig");

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
    /// Set by a test's last statement: a deinit before it is a failure's,
    /// and echoes the daemon's alert lines as the diagnostic.
    passed: bool = false,

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
        const argv = [_][]const u8{ self.bin, "mount", "-o", "interception", self.meta_uri, self.mp };
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

fn venueAvailable(io: Io) bool {
    if (linux.getuid() != 0) return false;
    const rc = linux.open("/dev/fuse", .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return false;
    _ = linux.close(@intCast(rc));
    var buf: [4096]u8 = undefined;
    const text = Io.Dir.cwd().readFile(io, "/proc/self/status", &buf) catch return false;
    var it = std.mem.splitScalar(u8, text, '\n');
    var cap_ok = false;
    while (it.next()) |line| {
        if (!std.mem.startsWith(u8, line, "CapEff:")) continue;
        const caps = std.fmt.parseInt(u64, std.mem.trim(u8, line["CapEff:".len..], " \t"), 16) catch return false;
        cap_ok = caps & (@as(u64, 1) << 21) != 0;
    }
    if (!cap_ok) return false;
    var st: linux.Statx = undefined;
    if (linux.errno(linux.statx(linux.AT.FDCWD, "squeezefs", 0, .{ .TYPE = true }, &st)) != .SUCCESS) {
        log.warn("il e2e: no ./squeezefs binary in the cwd — run through `zig build smoke`. Skipping.", .{});
        return false;
    }
    return true;
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

test "smoke: THE INTERCEPTION HOST — format → mount -o interception → the il0 xattr → a session in-process over the ONE client library → bind → ring pwrite 4 MiB (sever-at-dequeue) read back through the kernel path byte-exact → a 4 MiB stream of 1 MiB pieces places into ONE assembly adopted by ONE merge → a cold read then a warm read on the sync fast path (arena_copy_bytes ≡ bytes) → a W1-shaped 4 KiB overwrite serves INLINE on the dd lane (patch_edge_rmw_reads 0) → 8 no-wait tickets complete in offset order → the fd screen's refusals → kernel fsync → umount with the live session tears down cleanly (the client poisoned, no mount left)" {
    const io = testing.io;
    if (!venueAvailable(io)) return error.SkipZigTest;
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.formatSet();
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
    // The credential + the bound file: a regular file on the mount.
    const fd = try rig.openOnMount("ring.dat", .{ .ACCMODE = .RDWR, .CREAT = true, .CLOEXEC = true });
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
    // Read back through the KERNEL path (a second fd, buffered).
    const kfd = try rig.openOnMount("ring.dat", .{ .ACCMODE = .RDONLY, .CLOEXEC = true });
    defer _ = linux.close(kfd);
    const back = try gpa.alloc(u8, BLOCK);
    defer gpa.free(back);
    try testing.expectEqual(BLOCK, linux.pread(kfd, back.ptr, back.len, 0));
    try expectPattern(back, 0);

    // 2. Block 1 as ONE 4 MiB pwrite: the client stages 4 flights of 1 MiB
    //    under one doorbell, so the pass drains every sibling before its
    //    flush spawns the handoffs. The file is striped now and block 1 has
    //    no entry: the four chunks sever into ONE assembly, the first merge
    //    adopts it full, its siblings ride the adoption.
    const block1 = try gpa.alloc(u8, BLOCK);
    defer gpa.free(block1);
    pattern(block1, BLOCK);
    try testing.expectEqual(BLOCK, try served(s.pwrite(b.id, block1, BLOCK)));
    try testing.expectEqual(@as(u64, 8), try rig.stat("ipc_ops_write"));
    // ≥ 4: block 0's later chunks may have placed too once the first
    // handoff promoted the file (their assembly reaps unadopted: the first
    // merge of that block was a pooled one).
    try testing.expect((try rig.stat("ipc_placed_severs")) >= 4);
    try testing.expectEqual(@as(u64, 1), try rig.stat("ipc_placed_adoptions"));
    try testing.expectEqual(@as(u64, 0), try rig.stat("ipc_placed_adoption_refusals"));
    try testing.expectEqual(@as(u64, 0), try rig.stat("ipc_placed_sever_fallbacks"));
    try testing.expectEqual(BLOCK, linux.pread(kfd, back.ptr, back.len, @intCast(BLOCK)));
    try expectPattern(back, BLOCK);
    // Every parked block uploads (the write-through) before the W1 row.
    try rig.awaitStat("parked_buffers", 0);

    // 3. Reads on the sync fast path. A cold ALIGNED ring read is the
    //    handler's dest lease: a ranged DMA straight into the arena (zero
    //    daemon copies, never published — demoted, not warmed); a cold
    //    UNALIGNED read takes the whole-block walk, which fills the hot
    //    tier; the next read of that block serves WARM on the service
    //    thread with exactly its bytes on the arena copy ledger.
    const rbuf = try gpa.alloc(u8, 512 * 1024 + 100);
    defer gpa.free(rbuf);
    const demotes0 = try rig.stat("ipc_fast_path_miss_demotions");
    const dma0 = try rig.stat("read_dest_dma_bytes");
    try testing.expectEqual(@as(usize, 512 * 1024), try served(s.pread(b.id, rbuf[0 .. 512 * 1024], 1 * MiB)));
    try expectPattern(rbuf[0 .. 512 * 1024], 1 * MiB);
    try testing.expect((try rig.stat("read_dest_dma_bytes")) >= dma0 + 512 * 1024);
    try testing.expectEqual(rbuf.len, try served(s.pread(b.id, rbuf, 1 * MiB + 100)));
    try expectPattern(rbuf, 1 * MiB + 100);
    try testing.expectEqual(demotes0 + 2, try rig.stat("ipc_fast_path_miss_demotions"));
    const serves0 = try rig.stat("ipc_fast_path_serves");
    const copy0 = try rig.stat("ipc_read_arena_copy_bytes");
    const ops_read0 = try rig.stat("ipc_ops_read");
    try testing.expectEqual(@as(usize, 512 * 1024), try served(s.pread(b.id, rbuf[0 .. 512 * 1024], 1 * MiB)));
    try expectPattern(rbuf[0 .. 512 * 1024], 1 * MiB);
    try testing.expectEqual(serves0 + 1, try rig.stat("ipc_fast_path_serves"));
    try testing.expectEqual(copy0 + 512 * 1024, try rig.stat("ipc_read_arena_copy_bytes"));
    try testing.expectEqual(ops_read0 + 1, try rig.stat("ipc_ops_read"));
    try testing.expect((try rig.stat("ipc_read_demote_bytes")) >= 2 * 512 * 1024);
    // The engagement identity closes.
    {
        const text = try rig.stats();
        defer gpa.free(text);
        try testing.expect(std.mem.indexOf(u8, text, "\"closure\":{\"reads\":true,\"writes\":true,\"dd\":true}") != null);
    }

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
    //    (a foreign device) ⇒ mount.
    const ro = try rig.openOnMount("ring.dat", .{ .ACCMODE = .RDONLY, .CLOEXEC = true });
    defer _ = linux.close(ro);
    const rob = try s.bind(ro, .mount);
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
    s.unbind(rob.id);

    // 7. fsync through the kernel after the ring writes (the §5.6.3 order).
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.fsync(fd)));
    {
        const text = try rig.stats();
        defer gpa.free(text);
        try testing.expectEqual(@as(?u64, 0), numberAfter(text, "\"ipc_sessions_poisoned\":"));
        // Exactly the EBADF row above: the wrong-direction tripwire.
        try testing.expectEqual(@as(?u64, 1), numberAfter(text, "\"ipc_descriptor_rejects\":"));
        try testing.expect((numberAfter(text, "\"ipc_inval_notifies\":") orelse 0) >= 1);
    }

    // 8. `umount` with the LIVE session: the host poisons it (the client
    //    sees the generation bump), the daemon exits 0, no mount is left.
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
    if (!venueAvailable(io)) return error.SkipZigTest;
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
