//! Who holds a mount: the `fuser -m` census over `/proc`, keyed by the
//! kernel's MOUNT ID (never by path text — a renamed directory, a
//! `(deleted)` file or a bind of the same superblock elsewhere would lie).
//! Read by `squeezefs umount` before it signals the daemon and by the
//! daemon itself before it tears down (decision 0003 §S: an orderly unmount
//! of a busy mount is EBUSY, named, and nothing detaches). Linux only; the
//! line parsers are pure.
//!
//! What pins a mount (what makes a plain `umount2` EBUSY): a process's
//! cwd, root or exe on it, an open fd, a mapped file, a mount stacked
//! inside it. Per process: `cwd`/`root`/`exe` and every `fd/<n>` through
//! `statx(STATX_MNT_ID, AT_STATX_DONT_SYNC)` on the magic links (the
//! mount id of the target's path, resolved from the open file itself — no
//! lookup on the target filesystem; DONT_SYNC keeps a FUSE target from
//! issuing a GETATTR — the daemon runs this census on its own mount — and
//! an fd on a hung filesystem answers from its inode's cache), mappings
//! through `maps` by device number (the anon `0:<minor>` of the
//! superblock). NEVER through `fdinfo/<n>`: that is a seq_file whose read
//! generates the whole document — a fanotify or inotify instance prints
//! every mark (one desktop indexer's fd: 8.6 MB, 160–300 ms a read), an
//! epoll its registered fds, an io_uring its registered files and buffers
//! — so a census over the host's fds cost what the host's fds cost (0.4 s
//! idle, 0.7–2.6 s under a gate's load, three walks per `umount --lazy`:
//! 6.9 s on the train), not what the holders cost. One entry per process
//! (its first reference, in that order), one per stacked mount. A
//! per-thread cwd (`unshare(CLONE_FS)`) is not walked. Unreadable
//! processes are skipped — another user's under an unprivileged verb; a
//! non-dumpable one (a hardened daemon, a setuid program) when the caller
//! lacks CAP_SYS_PTRACE (root on a host has it; a container may not): the
//! kernel's `umount2` stays the authority on "busy"; this census is the
//! NAMES and the early answer. One process may be EXEMPT for its fds on
//! named inodes (`Exempt`): the daemon's census exempts the stop's own
//! sender's control-document reads — `squeezefs umount` opens `.stats` on
//! its signal's heels for the daemon's verdict, and that read must not be
//! the holder that refuses its own unmount (its cwd, root, exe, mappings
//! and every other fd still hold).

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const fuzzkit = @import("../core/fuzzkit.zig");
const log = @import("../core/log.zig");
const testkit = @import("../core/testkit.zig");
const mountinfo = @import("../core/mountinfo.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/holders is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

/// Holders NAMED in a refusal line; the rest are counted (`+N more`) — a
/// console line that names eight processes is readable, one that names
/// a thousand is not.
pub const MAX_NAMED: usize = 8;

/// `/proc/<pid>/comm` is `TASK_COMM_LEN` (16) bytes with its NUL.
pub const COMM_MAX: usize = 16;

/// A process's `maps` beyond this is read no further (a warning names
/// it): 16 MiB is ~200k mappings — the default `vm.max_map_count` is
/// 65530 at ~150 bytes a line — past any process that is not itself a
/// fault.
pub const MAPS_READ_LIMIT: Io.Limit = .limited(16 << 20);

pub const Via = enum {
    cwd,
    root,
    exe,
    fd,
    map,
    submount,

    pub fn text(self: Via) []const u8 {
        return switch (self) {
            .cwd => "cwd",
            .root => "root",
            .exe => "exe",
            .fd => "fd",
            .map => "mmap",
            .submount => "a mount stacked at",
        };
    }
};

pub const Holder = struct {
    /// 0 for a stacked mount.
    pid: linux.pid_t,
    comm: [COMM_MAX]u8 = @splat(0),
    comm_len: u8 = 0,
    via: Via,
    /// The fd number for `.fd`; 0 otherwise.
    fd: u32 = 0,
    /// The stacked mount's path for `.submount` (a slice into the
    /// mountinfo text the census was run over); empty otherwise.
    path: []const u8 = "",

    pub fn format(self: Holder, w: *Io.Writer) Io.Writer.Error!void {
        switch (self.via) {
            .submount => try w.print("{s} {s}", .{ self.via.text(), self.path }),
            .fd => try w.print("pid {d} ({s}) fd {d}", .{ self.pid, self.comm[0..self.comm_len], self.fd }),
            else => try w.print("pid {d} ({s}) {s}", .{ self.pid, self.comm[0..self.comm_len], self.via.text() }),
        }
    }
};

/// The census: the first `MAX_NAMED` holders by name, the total.
pub const Census = struct {
    named: [MAX_NAMED]Holder = undefined,
    n_named: usize = 0,
    total: usize = 0,

    pub fn busy(self: Census) bool {
        return self.total > 0;
    }

    fn add(self: *Census, h: Holder) void {
        if (self.n_named < MAX_NAMED) {
            self.named[self.n_named] = h;
            self.n_named += 1;
        }
        self.total += 1;
    }

    /// `N holder(s): pid 12 (bash) cwd, pid 40 (vim) fd 3, +2 more`.
    pub fn format(self: Census, w: *Io.Writer) Io.Writer.Error!void {
        try w.print("{d} holder(s): ", .{self.total});
        for (self.named[0..self.n_named], 0..) |h, i| {
            if (i != 0) try w.writeAll(", ");
            try h.format(w);
        }
        if (self.total > self.n_named) try w.print(", +{d} more", .{self.total - self.n_named});
    }
};

/// The mount under judgement, from its mountinfo entry.
pub const Target = struct {
    mount_id: u64,
    dev_major: u32,
    dev_minor: u32,

    pub fn of(e: mountinfo.Entry) Target {
        return .{ .mount_id = e.id, .dev_major = e.dev_major, .dev_minor = e.dev_minor };
    }
};

/// The one process whose fds on the inodes `fd_ino` accepts hold nothing
/// (module doc).
pub const Exempt = struct {
    pid: linux.pid_t,
    fd_ino: *const fn (ino: u64) bool,
};

/// The device of one `/proc/<pid>/maps` line (`addr perms offset
/// MAJ:MIN inode [path]`, the device in hex); null for an anonymous or
/// malformed line.
pub fn parseMapsDevice(line: []const u8) ?struct { major: u32, minor: u32 } {
    var it = std.mem.tokenizeScalar(u8, line, ' ');
    _ = it.next() orelse return null;
    _ = it.next() orelse return null;
    _ = it.next() orelse return null;
    const dev = it.next() orelse return null;
    const colon = std.mem.indexOfScalar(u8, dev, ':') orelse return null;
    const major = std.fmt.parseInt(u32, dev[0..colon], 16) catch return null;
    const minor = std.fmt.parseInt(u32, dev[colon + 1 ..], 16) catch return null;
    const inode = it.next() orelse return null;
    // The anonymous device 00:00 with inode 0 is no file.
    if (major == 0 and minor == 0 and std.mem.eql(u8, inode, "0")) return null;
    return .{ .major = major, .minor = minor };
}

fn skipped(skip: []const linux.pid_t, pid: linux.pid_t) bool {
    for (skip) |s| if (s == pid) return true;
    return false;
}

/// What a magic link points at: the mount id of the target's path and
/// its inode number; null when the link is unreadable (a kernel thread,
/// another user's process, an fd closed meanwhile) or the kernel reports
/// no mount id.
const LinkTarget = struct { mnt_id: u64, ino: u64 };

fn linkTarget(link: [*:0]const u8) ?LinkTarget {
    var st: linux.Statx = undefined;
    const rc = linux.statx(linux.AT.FDCWD, link, linux.AT.STATX_DONT_SYNC, .{ .MNT_ID = true, .INO = true }, &st);
    if (linux.errno(rc) != .SUCCESS or !st.mask.MNT_ID) return null;
    return .{ .mnt_id = st.mnt_id, .ino = st.ino };
}

/// A proc file whole (owned). It reports size 0, so it is read until EOF,
/// never sized by stat (`readFileAlloc` answers it empty); `StreamTooLong`
/// at `limit`.
fn readProcFileAlloc(gpa: std.mem.Allocator, io: Io, path: []const u8, limit: Io.Limit) ![]u8 {
    var file = try Io.Dir.cwd().openFile(io, path, .{});
    defer file.close(io);
    var reader = file.readerStreaming(io, &.{});
    return reader.interface.allocRemaining(gpa, limit) catch |e| switch (e) {
        error.ReadFailed => return reader.err.?,
        error.OutOfMemory, error.StreamTooLong => |x| return x,
    };
}

fn readComm(io: Io, pid: linux.pid_t, h: *Holder) void {
    var pb: [48]u8 = undefined;
    var buf: [COMM_MAX + 1]u8 = undefined;
    const p = std.fmt.bufPrint(&pb, "/proc/{d}/comm", .{pid}) catch return;
    const text = Io.Dir.cwd().readFile(io, p, &buf) catch return;
    const t = std.mem.trimEnd(u8, text, "\n");
    const n = @min(t.len, COMM_MAX);
    @memcpy(h.comm[0..n], t[0..n]);
    h.comm_len = @intCast(n);
}

/// The process's first reference into the target, in the module doc's
/// order; null when it holds nothing (or nothing readable).
fn processHolds(gpa: std.mem.Allocator, io: Io, pid: linux.pid_t, target: Target, exempt_ino: ?*const fn (ino: u64) bool) ?Holder {
    var pb: [64:0]u8 = undefined;
    inline for (.{ .cwd, .root, .exe }) |via| {
        const link = std.fmt.bufPrintSentinel(&pb, "/proc/{d}/{s}", .{ pid, @tagName(via) }, 0) catch return null;
        if (linkTarget(link.ptr)) |t| if (t.mnt_id == target.mount_id) return .{ .pid = pid, .via = via };
    }
    // Every fd through its magic link's mount id (module doc: never the
    // fdinfo document).
    {
        const fd_dir = std.fmt.bufPrint(&pb, "/proc/{d}/fd", .{pid}) catch return null;
        if (Io.Dir.cwd().openDir(io, fd_dir, .{ .iterate = true })) |d| {
            var dir = d;
            defer dir.close(io);
            var it = dir.iterate();
            while (it.next(io) catch null) |ent| {
                const fd = std.fmt.parseInt(u32, ent.name, 10) catch continue; // the fd number, decimal
                var lb: [64:0]u8 = undefined;
                const link = std.fmt.bufPrintSentinel(&lb, "/proc/{d}/fd/{d}", .{ pid, fd }, 0) catch continue;
                const t = linkTarget(link.ptr) orelse continue;
                if (t.mnt_id != target.mount_id) continue;
                if (exempt_ino) |exempt| if (exempt(t.ino)) continue;
                return .{ .pid = pid, .via = .fd, .fd = fd };
            }
        } else |_| {}
    }
    // Mappings by device number.
    {
        const maps = std.fmt.bufPrint(&pb, "/proc/{d}/maps", .{pid}) catch return null;
        const text = readProcFileAlloc(gpa, io, maps, MAPS_READ_LIMIT) catch |e| {
            // Past the cap the mappings go unjudged: said, never silent
            // (the kernel's umount2 still answers EBUSY for them).
            if (e == error.StreamTooLong) log.warn("holders: /proc/{d}/maps exceeds {d} bytes; its mappings are not judged", .{ pid, MAPS_READ_LIMIT.toInt().? });
            return null;
        };
        defer gpa.free(text);
        var lines = std.mem.splitScalar(u8, text, '\n');
        while (lines.next()) |line| {
            const dev = parseMapsDevice(line) orelse continue;
            if (dev.major == target.dev_major and dev.minor == target.dev_minor) return .{ .pid = pid, .via = .map };
        }
    }
    return null;
}

/// The census of `target` over every process but `skip` (the caller, the
/// daemon), `exempt`'s named fds aside, plus the mounts stacked inside it
/// (`mountinfo_text` is the table the target was read from; a submount's
/// path slices into it).
pub fn census(gpa: std.mem.Allocator, io: Io, target: Target, mountinfo_text: []const u8, skip: []const linux.pid_t, exempt: ?Exempt) Census {
    var out: Census = .{};
    {
        var lines = std.mem.splitScalar(u8, mountinfo_text, '\n');
        while (lines.next()) |line| {
            const e = mountinfo.parseLine(line) orelse continue;
            if (e.parent_id == target.mount_id) out.add(.{ .pid = 0, .via = .submount, .path = e.mountpoint });
        }
    }
    var proc_dir = Io.Dir.cwd().openDir(io, "/proc", .{ .iterate = true }) catch return out;
    defer proc_dir.close(io);
    var it = proc_dir.iterate();
    while (it.next(io) catch null) |ent| {
        const pid = std.fmt.parseInt(linux.pid_t, ent.name, 10) catch continue; // a pid directory, decimal
        if (skipped(skip, pid)) continue;
        const exempt_ino: ?*const fn (ino: u64) bool = if (exempt) |e| (if (e.pid == pid) e.fd_ino else null) else null;
        if (processHolds(gpa, io, pid, target, exempt_ino)) |held| {
            var h = held;
            readComm(io, pid, &h);
            out.add(h);
        }
    }
    return out;
}

/// The census of the mount at `mountpoint` (null when nothing is mounted
/// there): the table is read once, the target taken from it.
pub fn censusAt(gpa: std.mem.Allocator, io: Io, mountpoint: []const u8, skip: []const linux.pid_t, exempt: ?Exempt) ?Census {
    const buf = gpa.alloc(u8, mountinfo.READ_BYTES) catch return null;
    defer gpa.free(buf);
    const text = mountinfo.read(io, buf);
    const e = mountinfo.entryAt(text, mountpoint) orelse return null;
    return census(gpa, io, Target.of(e), text, skip, exempt);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

const testing = std.testing;

test "maps: the device in hex; an anonymous mapping (00:00 inode 0) and a short line are null" {
    const d = parseMapsDevice("ffff8000-ffff9000 r-xp 00000000 00:2f 1234  /build/tmp/mnt-7/lib.so").?;
    try testing.expectEqual(@as(u32, 0), d.major);
    try testing.expectEqual(@as(u32, 47), d.minor);
    const e = parseMapsDevice("aaaa-bbbb rw-p 00001000 fd:01 99 /usr/lib/x").?;
    try testing.expectEqual(@as(u32, 253), e.major);
    try testing.expectEqual(@as(u32, 1), e.minor);
    try testing.expect(parseMapsDevice("aaaa-bbbb rw-p 00000000 00:00 0") == null);
    try testing.expect(parseMapsDevice("aaaa-bbbb rw-p") == null);
    try testing.expect(parseMapsDevice("") == null);
}

test "the census line names up to MAX_NAMED and counts the rest" {
    var c: Census = .{};
    var i: usize = 0;
    while (i < MAX_NAMED + 2) : (i += 1) {
        var h: Holder = .{ .pid = @intCast(100 + i), .via = if (i == 0) .fd else .cwd, .fd = 7 };
        const name = "bash";
        @memcpy(h.comm[0..name.len], name);
        h.comm_len = name.len;
        c.add(h);
    }
    c.add(.{ .pid = 0, .via = .submount, .path = "/m/sub" });
    var buf: [512]u8 = undefined;
    var w: Io.Writer = .fixed(&buf);
    try c.format(&w);
    const line = w.buffered();
    try testing.expect(std.mem.startsWith(u8, line, "11 holder(s): pid 100 (bash) fd 7, pid 101 (bash) cwd, "));
    try testing.expect(std.mem.endsWith(u8, line, ", +3 more"));
    try testing.expectEqual(MAX_NAMED, c.n_named);
    try testing.expect(c.busy());
    try testing.expect(!(Census{}).busy());
}

fn linesFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [512]u8 = undefined;
    const len = smith.slice(&buf);
    _ = parseMapsDevice(buf[0..len]);
}

test "fuzz: the maps line parser is total on arbitrary text" {
    try fuzzkit.fuzz({}, linesFuzz, .{ .corpus = &.{ fuzzkit.slice("a-b r-xp 0 00:2f 1 /x"), fuzzkit.slice("aaaa-bbbb rw-p 00000000 00:00 0") } });
}

test "a proc file is read to EOF, not to its stat size (0): /proc/self/maps comes back whole; the cap is StreamTooLong, never a short read" {
    const io = testing.io;
    const gpa = testing.allocator;
    const whole = try readProcFileAlloc(gpa, io, "/proc/self/maps", MAPS_READ_LIMIT);
    defer gpa.free(whole);
    // The main thread's stack is among the last lines, far past the first
    // read's chunk; a complete last record ends the text.
    try testing.expect(std.mem.indexOf(u8, whole, "[stack]") != null);
    try testing.expect(std.mem.endsWith(u8, whole, "\n"));
    try testing.expectError(error.StreamTooLong, readProcFileAlloc(gpa, io, "/proc/self/maps", .limited(64)));
}

/// The exemption row's inode (its predicate is a plain function pointer).
var exempt_ino_for_test: u64 = 0;

fn isExemptInoForTest(ino: u64) bool {
    return ino == exempt_ino_for_test;
}

test "the census over a real mount: a child whose cwd is a tmpfs we mount is found by cwd (then by fd once it leaves the directory); an exempt process's fd on its named inode holds nothing while its other fd does; nobody holds it after the child dies and umount2 succeeds" {
    if (linux.geteuid() != 0) return testkit.skip(@src(), .no_root, "mount(2) of a tmpfs needs root");
    const io = testing.io;
    const gpa = testing.allocator;
    _ = linux.mkdir("/build/tmp", 0o755);
    var pb: [64:0]u8 = undefined;
    const mp = try std.fmt.bufPrintSentinel(&pb, "/build/tmp/holders-{d}", .{linux.getpid()}, 0);
    _ = linux.mkdir(mp.ptr, 0o755);
    defer _ = linux.rmdir(mp.ptr);
    if (linux.errno(linux.mount("tmpfs", mp.ptr, "tmpfs", 0, 0)) != .SUCCESS) return testkit.skip(@src(), .no_cap, "mount(2) of a tmpfs refused (CAP_SYS_ADMIN)");
    defer _ = linux.umount2(mp.ptr, linux.MNT.DETACH);
    const self_pid = linux.getpid();
    // Nobody yet.
    {
        const c = censusAt(gpa, io, mp, &.{self_pid}, null).?;
        try testing.expectEqual(@as(usize, 0), c.total);
    }
    // A child parks with the tmpfs as its cwd; a pipe tells us it is there.
    var fds: [2]linux.fd_t = undefined;
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.pipe2(&fds, .{ .CLOEXEC = true })));
    var done: [2]linux.fd_t = undefined;
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.pipe2(&done, .{ .CLOEXEC = true })));
    const rc = linux.fork();
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc));
    if (rc == 0) {
        // The child drops its copies of the ends it does not use, else its
        // own `done[1]` keeps the pipe from ever reading EOF. Dumpable
        // again: an earlier test hardened this process (`PR_SET_DUMPABLE`
        // 0), and the magic links of a non-dumpable task need
        // CAP_SYS_PTRACE, which the venue container does not grant root.
        _ = linux.close(fds[0]);
        _ = linux.close(done[1]);
        _ = linux.prctl(@backingInt(linux.PR.SET_DUMPABLE), 1, 0, 0, 0);
        _ = linux.chdir(mp.ptr);
        var one: u8 = 1;
        _ = linux.write(fds[1], @ptrCast(&one), 1);
        _ = linux.close(fds[1]);
        // Park until the parent closes the `done` pipe's write end.
        _ = linux.read(done[0], @ptrCast(&one), 1);
        linux.exit(0);
    }
    const child: linux.pid_t = @intCast(rc);
    _ = linux.close(fds[1]);
    var byte: u8 = 0;
    _ = linux.read(fds[0], @ptrCast(&byte), 1);
    _ = linux.close(fds[0]);
    {
        const c = censusAt(gpa, io, mp, &.{self_pid}, null).?;
        try testing.expectEqual(@as(usize, 1), c.total);
        try testing.expectEqual(child, c.named[0].pid);
        try testing.expectEqual(Via.cwd, c.named[0].via);
        try testing.expectEqual(linux.E.BUSY, linux.errno(linux.umount2(mp.ptr, 0)));
        var buf: [256]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        try c.format(&w);
        try testing.expect(std.mem.indexOf(u8, w.buffered(), ") cwd") != null);
    }
    // Our own open fd on it is a holder too, found by fd when we are not skipped.
    {
        var fb: [96:0]u8 = undefined;
        const fpath = try std.fmt.bufPrintSentinel(&fb, "{s}/f", .{mp}, 0);
        const frc = linux.open(fpath.ptr, .{ .ACCMODE = .RDWR, .CREAT = true, .CLOEXEC = true }, 0o600);
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(frc));
        const fd: linux.fd_t = @intCast(frc);
        const c = censusAt(gpa, io, mp, &.{}, null).?;
        try testing.expectEqual(@as(usize, 2), c.total);
        var saw_fd = false;
        for (c.named[0..c.n_named]) |h| if (h.pid == self_pid and h.via == .fd and h.fd == @as(u32, @intCast(fd))) {
            saw_fd = true;
        };
        try testing.expect(saw_fd);
        // Exempt for its fds on THAT inode, we hold nothing through it — a
        // second file's fd still holds; the child's cwd holds regardless.
        var st: linux.Statx = undefined;
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.statx(fd, "", linux.AT.EMPTY_PATH, .{ .INO = true }, &st)));
        exempt_ino_for_test = st.ino;
        const exempt: Exempt = .{ .pid = self_pid, .fd_ino = &isExemptInoForTest };
        try testing.expectEqual(@as(usize, 1), censusAt(gpa, io, mp, &.{}, exempt).?.total);
        var gb: [96:0]u8 = undefined;
        const gpath = try std.fmt.bufPrintSentinel(&gb, "{s}/g", .{mp}, 0);
        const grc = linux.open(gpath.ptr, .{ .ACCMODE = .RDWR, .CREAT = true, .CLOEXEC = true }, 0o600);
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(grc));
        const c2 = censusAt(gpa, io, mp, &.{}, exempt).?;
        try testing.expectEqual(@as(usize, 2), c2.total);
        var saw_g = false;
        for (c2.named[0..c2.n_named]) |h| if (h.pid == self_pid and h.via == .fd and h.fd == @as(u32, @intCast(grc))) {
            saw_g = true;
        };
        try testing.expect(saw_g);
        _ = linux.close(@intCast(grc));
        _ = linux.close(fd);
    }
    // The child leaves: nobody holds it, the plain umount succeeds.
    _ = linux.close(done[1]);
    _ = linux.close(done[0]);
    var status: i32 = 0;
    _ = linux.waitpid(child, &status, 0);
    {
        const c = censusAt(gpa, io, mp, &.{self_pid}, null).?;
        try testing.expectEqual(@as(usize, 0), c.total);
    }
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.umount2(mp.ptr, 0)));
    try testing.expect(censusAt(gpa, io, mp, &.{}, null) == null);
}

test "the census over a real mount: a child holding ONLY a mapping of a file on a tmpfs we mount (no fd on it, cwd elsewhere) is found by mmap — /proc/<pid>/maps reports size 0 and must be read to EOF; after it dies nobody holds it" {
    if (linux.geteuid() != 0) return testkit.skip(@src(), .no_root, "mount(2) of a tmpfs needs root");
    const io = testing.io;
    const gpa = testing.allocator;
    _ = linux.mkdir("/build/tmp", 0o755);
    var pb: [64:0]u8 = undefined;
    const mp = try std.fmt.bufPrintSentinel(&pb, "/build/tmp/holders-map-{d}", .{linux.getpid()}, 0);
    _ = linux.mkdir(mp.ptr, 0o755);
    defer _ = linux.rmdir(mp.ptr);
    if (linux.errno(linux.mount("tmpfs", mp.ptr, "tmpfs", 0, 0)) != .SUCCESS) return testkit.skip(@src(), .no_cap, "mount(2) of a tmpfs refused (CAP_SYS_ADMIN)");
    defer _ = linux.umount2(mp.ptr, linux.MNT.DETACH);
    const self_pid = linux.getpid();
    // One page of a file on it, nothing open by the time the census runs.
    var fb: [96:0]u8 = undefined;
    const fpath = try std.fmt.bufPrintSentinel(&fb, "{s}/f", .{mp}, 0);
    {
        const frc = linux.open(fpath.ptr, .{ .ACCMODE = .RDWR, .CREAT = true, .CLOEXEC = true }, 0o600);
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(frc));
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.ftruncate(@intCast(frc), 4096)));
        _ = linux.close(@intCast(frc));
    }
    var fds: [2]linux.fd_t = undefined;
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.pipe2(&fds, .{ .CLOEXEC = true })));
    var done: [2]linux.fd_t = undefined;
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.pipe2(&done, .{ .CLOEXEC = true })));
    const rc = linux.fork();
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc));
    if (rc == 0) {
        // As the cwd test's child: drop the unused pipe ends, dumpable again
        // (`maps` of a non-dumpable task needs CAP_SYS_PTRACE). Map the
        // file, then CLOSE its fd: the mapping alone holds the mount.
        _ = linux.close(fds[0]);
        _ = linux.close(done[1]);
        _ = linux.prctl(@backingInt(linux.PR.SET_DUMPABLE), 1, 0, 0, 0);
        var one: u8 = 1;
        const frc = linux.open(fpath.ptr, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
        if (linux.errno(frc) != .SUCCESS) linux.exit(2);
        const m = linux.mmap(null, 4096, .{ .READ = true }, .{ .TYPE = .SHARED }, @intCast(frc), 0);
        if (linux.errno(m) != .SUCCESS) linux.exit(3);
        _ = linux.close(@intCast(frc));
        _ = linux.write(fds[1], @ptrCast(&one), 1);
        _ = linux.close(fds[1]);
        _ = linux.read(done[0], @ptrCast(&one), 1);
        linux.exit(0);
    }
    const child: linux.pid_t = @intCast(rc);
    _ = linux.close(fds[1]);
    var byte: u8 = 0;
    _ = linux.read(fds[0], @ptrCast(&byte), 1);
    _ = linux.close(fds[0]);
    {
        const c = censusAt(gpa, io, mp, &.{self_pid}, null).?;
        try testing.expectEqual(linux.E.BUSY, linux.errno(linux.umount2(mp.ptr, 0)));
        try testing.expectEqual(@as(usize, 1), c.total);
        try testing.expectEqual(child, c.named[0].pid);
        try testing.expectEqual(Via.map, c.named[0].via);
        var buf: [256]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        try c.format(&w);
        try testing.expect(std.mem.indexOf(u8, w.buffered(), ") mmap") != null);
    }
    _ = linux.close(done[1]);
    _ = linux.close(done[0]);
    var status: i32 = 0;
    _ = linux.waitpid(child, &status, 0);
    try testing.expectEqual(@as(i32, 0), status);
    {
        const c = censusAt(gpa, io, mp, &.{self_pid}, null).?;
        try testing.expectEqual(@as(usize, 0), c.total);
    }
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.umount2(mp.ptr, 0)));
}
