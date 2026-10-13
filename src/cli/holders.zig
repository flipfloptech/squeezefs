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
//! inside it. Per process: `cwd`/`root`/`exe` through `statx(STATX_MNT_ID,
//! AT_STATX_DONT_SYNC)` on the magic links (the mount id of the target's
//! path; DONT_SYNC keeps a FUSE target from issuing a GETATTR — the
//! daemon runs this census on its own mount), every fd through
//! `fdinfo/<n>`'s `mnt_id:` line (a text read, no path resolution — an fd
//! on a hung filesystem costs nothing), mappings through `maps` by device
//! number (the anon `0:<minor>` of the superblock). One entry per process
//! (its first reference, in that order), one per stacked mount. A
//! per-thread cwd (`unshare(CLONE_FS)`) is not walked. Unreadable
//! processes are skipped — another user's under an unprivileged verb; a
//! non-dumpable one (a hardened daemon, a setuid program) when the caller
//! lacks CAP_SYS_PTRACE (root on a host has it; a container may not): the
//! kernel's `umount2` stays the authority on "busy"; this census is the
//! NAMES and the early answer.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const fuzzkit = @import("../core/fuzzkit.zig");
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

/// A process's `maps` beyond this is read no further: 16 MiB is ~200k
/// mappings, past any process that is not itself a fault.
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

/// `mnt_id:\t<n>` out of a `/proc/<pid>/fdinfo/<fd>` document.
pub fn parseFdinfoMountId(text: []const u8) ?u64 {
    var lines = std.mem.splitScalar(u8, text, '\n');
    while (lines.next()) |line| {
        if (!std.mem.startsWith(u8, line, "mnt_id:")) continue;
        const v = std.mem.trim(u8, line["mnt_id:".len..], " \t\r");
        return std.fmt.parseInt(u64, v, 10) catch null; // decimal, as proc(5) prints it
    }
    return null;
}

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

/// The mount id of the path a magic link points at; null when the link is
/// unreadable (a kernel thread, another user's process) or the kernel
/// reports no mount id.
fn linkMountId(link: [*:0]const u8) ?u64 {
    var st: linux.Statx = undefined;
    const rc = linux.statx(linux.AT.FDCWD, link, linux.AT.STATX_DONT_SYNC, .{ .MNT_ID = true }, &st);
    if (linux.errno(rc) != .SUCCESS or !st.mask.MNT_ID) return null;
    return st.mnt_id;
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
fn processHolds(gpa: std.mem.Allocator, io: Io, pid: linux.pid_t, target: Target) ?Holder {
    var pb: [64:0]u8 = undefined;
    inline for (.{ .cwd, .root, .exe }) |via| {
        const link = std.fmt.bufPrintSentinel(&pb, "/proc/{d}/{s}", .{ pid, @tagName(via) }, 0) catch return null;
        if (linkMountId(link.ptr)) |id| if (id == target.mount_id) return .{ .pid = pid, .via = via };
    }
    // Every fd through fdinfo's mount id.
    {
        const fd_dir = std.fmt.bufPrint(&pb, "/proc/{d}/fd", .{pid}) catch return null;
        if (Io.Dir.cwd().openDir(io, fd_dir, .{ .iterate = true })) |d| {
            var dir = d;
            defer dir.close(io);
            var it = dir.iterate();
            while (it.next(io) catch null) |ent| {
                const fd = std.fmt.parseInt(u32, ent.name, 10) catch continue; // the fd number, decimal
                var ib: [64]u8 = undefined;
                var fb: [512]u8 = undefined;
                const info = std.fmt.bufPrint(&ib, "/proc/{d}/fdinfo/{d}", .{ pid, fd }) catch continue;
                const text = Io.Dir.cwd().readFile(io, info, &fb) catch continue;
                if (parseFdinfoMountId(text)) |id| if (id == target.mount_id) return .{ .pid = pid, .via = .fd, .fd = fd };
            }
        } else |_| {}
    }
    // Mappings by device number.
    {
        const maps = std.fmt.bufPrint(&pb, "/proc/{d}/maps", .{pid}) catch return null;
        const text = Io.Dir.cwd().readFileAlloc(io, maps, gpa, MAPS_READ_LIMIT) catch return null;
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
/// daemon), plus the mounts stacked inside it (`mountinfo_text` is the
/// table the target was read from; a submount's path slices into it).
pub fn census(gpa: std.mem.Allocator, io: Io, target: Target, mountinfo_text: []const u8, skip: []const linux.pid_t) Census {
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
        if (processHolds(gpa, io, pid, target)) |held| {
            var h = held;
            readComm(io, pid, &h);
            out.add(h);
        }
    }
    return out;
}

/// The census of the mount at `mountpoint` (null when nothing is mounted
/// there): the table is read once, the target taken from it.
pub fn censusAt(gpa: std.mem.Allocator, io: Io, mountpoint: []const u8, skip: []const linux.pid_t) ?Census {
    const buf = gpa.alloc(u8, mountinfo.READ_BYTES) catch return null;
    defer gpa.free(buf);
    const text = mountinfo.read(io, buf);
    const e = mountinfo.entryAt(text, mountpoint) orelse return null;
    return census(gpa, io, Target.of(e), text, skip);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

const testing = std.testing;

test "fdinfo: the mnt_id line; a document without one is null" {
    try testing.expectEqual(@as(?u64, 45), parseFdinfoMountId("pos:\t0\nflags:\t02100002\nmnt_id:\t45\nino:\t1\n"));
    try testing.expectEqual(@as(?u64, null), parseFdinfoMountId("pos:\t0\nflags:\t02\n"));
    try testing.expectEqual(@as(?u64, null), parseFdinfoMountId("mnt_id:\tx\n"));
}

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
    _ = parseFdinfoMountId(buf[0..len]);
    _ = parseMapsDevice(buf[0..len]);
}

test "fuzz: the fdinfo and maps line parsers are total on arbitrary text" {
    try fuzzkit.fuzz({}, linesFuzz, .{ .corpus = &.{ fuzzkit.slice("mnt_id:\t45\n"), fuzzkit.slice("a-b r-xp 0 00:2f 1 /x") } });
}

test "the census over a real mount: a child whose cwd is a tmpfs we mount is found by cwd (then by fd once it leaves the directory); nobody holds it after the child dies and umount2 succeeds" {
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
        const c = censusAt(gpa, io, mp, &.{self_pid}).?;
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
        const c = censusAt(gpa, io, mp, &.{self_pid}).?;
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
        const c = censusAt(gpa, io, mp, &.{}).?;
        try testing.expectEqual(@as(usize, 2), c.total);
        var saw_fd = false;
        for (c.named[0..c.n_named]) |h| if (h.pid == self_pid and h.via == .fd and h.fd == @as(u32, @intCast(fd))) {
            saw_fd = true;
        };
        try testing.expect(saw_fd);
        _ = linux.close(fd);
    }
    // The child leaves: nobody holds it, the plain umount succeeds.
    _ = linux.close(done[1]);
    _ = linux.close(done[0]);
    var status: i32 = 0;
    _ = linux.waitpid(child, &status, 0);
    {
        const c = censusAt(gpa, io, mp, &.{self_pid}).?;
        try testing.expectEqual(@as(usize, 0), c.total);
    }
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.umount2(mp.ptr, 0)));
    try testing.expect(censusAt(gpa, io, mp, &.{}) == null);
}
