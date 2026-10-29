//! The shim's syscall face (decision 0003 §F "Shim syscalls"): raw
//! `std.os.linux` wrappers ONLY, plus the local declarations std lacks
//! (`fstatfs`, `FUSE_SUPER_MAGIC`, `RTLD_NEXT`, `dlvsym`, the errno cell).
//! With libc linked, `std.posix.*` dispatches to `std.c.*`, whose PLT
//! entries resolve to the shim's OWN exported `read`/`write`/`close`/… —
//! a recursion into the interposer. Nothing in `src/shim/` may call
//! `std.posix`, `std.fs`, `std.Io` or `std.c` I/O; the three libc calls that
//! stay libc (`dlsym`/`dlvsym`, `pthread_atfork`) are declared here and
//! made only under the reentrancy guard.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;

pub const fd_t = linux.fd_t;
pub const E = linux.E;

// ---------------------------------------------------------------------------
// errno.
// ---------------------------------------------------------------------------

/// glibc's errno cell (`__errno_location`), declared locally: the shim sets
/// errno exactly as the real call would on a served failure.
extern "c" fn __errno_location() *c_int;

pub fn setErrno(e: E) void {
    __errno_location().* = @intCast(@backingInt(e));
}

pub fn getErrno() E {
    return @fromBackingInt(@intCast(@as(u16, @intCast(__errno_location().*))));
}

/// A raw syscall's `-errno` return turned into the libc convention
/// (`errno` set, −1), for the synthesize-the-kernel-answer arm.
pub fn libcResult(rc: usize) isize {
    const e = linux.errno(rc);
    if (e != .SUCCESS) {
        setErrno(e);
        return -1;
    }
    return @bitCast(rc);
}

/// The libaio convention: 0-or-count, NEGATIVE errno (never −1/errno).
pub fn aioResult(rc: usize) c_int {
    const e = linux.errno(rc);
    if (e != .SUCCESS) return -@as(c_int, @intCast(@backingInt(e)));
    return @intCast(rc);
}

// ---------------------------------------------------------------------------
// The dynamic loader (the ONLY libc entry points besides pthread_atfork).
// ---------------------------------------------------------------------------

/// glibc's `RTLD_NEXT` = `(void*)-1` (`std.c` carries no constant): the
/// next object after the caller in the link map — position-relative, so
/// the resolution is correct wherever the shim sits (preload or linked).
pub const RTLD_NEXT: *anyopaque = @ptrFromInt(std.math.maxInt(usize));

pub const dlsym = std.c.dlsym;
/// GNU extension absent from `std.c`: `dlsym(RTLD_NEXT)` on glibc < 2.36
/// answers the BASE version of a multi-versioned symbol (EL8's 4-argument
/// `io_getevents@LIBAIO_0.1`), so libaio's symbols resolve by version.
pub extern "c" fn dlvsym(handle: ?*anyopaque, symbol: [*:0]const u8, version: [*:0]const u8) ?*anyopaque;
pub const pthread_atfork = std.c.pthread_atfork;
pub const dl_iterate_phdr = std.c.dl_iterate_phdr;
pub const dl_phdr_info = std.c.dl_phdr_info;
pub const getauxval = std.c.getauxval;
/// `AT_SECURE`: the loader ignored `LD_PRELOAD` (setuid/AT_SECURE image).
pub const AT_SECURE: c_ulong = 23;

// ---------------------------------------------------------------------------
// statfs / FUSE.
// ---------------------------------------------------------------------------

/// Linux `statfs.f_type` for FUSE (`fs/fuse/inode.c` `FUSE_SUPER_MAGIC`);
/// not in std.
pub const FUSE_SUPER_MAGIC: i64 = 0x65735546;

/// `struct statfs` on the 64-bit ABIs (x86_64, aarch64): `f_type` is the
/// first word; the rest is read by nothing here but must size the buffer
/// the kernel fills (120 B on both targets).
pub const Statfs = extern struct {
    f_type: i64,
    f_bsize: i64,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_fsid: [2]i32,
    f_namelen: i64,
    f_frsize: i64,
    f_flags: i64,
    f_spare: [4]i64,
};

comptime {
    std.debug.assert(@sizeOf(Statfs) == 120);
    std.debug.assert(@offsetOf(Statfs, "f_type") == 0);
}

/// std has no `fstatfs` wrapper.
pub fn fstatfs(fd: fd_t, buf: *Statfs) E {
    return linux.errno(linux.syscall2(.fstatfs, @as(usize, @bitCast(@as(isize, fd))), @intFromPtr(buf)));
}

/// `f_type` of the filesystem behind `fd`, null on failure.
pub fn fsType(fd: fd_t) ?i64 {
    var st: Statfs = undefined;
    if (fstatfs(fd, &st) != .SUCCESS) return null;
    return st.f_type;
}

// ---------------------------------------------------------------------------
// The fd facts the ladder needs in ONE statx.
// ---------------------------------------------------------------------------

pub const FileKind = enum { regular, directory, other };

pub const Stat = struct {
    kind: FileKind,
    dev: u64,
    ino: u64,
    nlink: u32,
    uid: u32,
};

fn makedev(major: u32, minor: u32) u64 {
    return (@as(u64, major) << 8) | minor | (@as(u64, major & ~@as(u32, 0xfff)) << 32) | (@as(u64, minor & ~@as(u32, 0xff)) << 12);
}

pub fn fstat(fd: fd_t) ?Stat {
    var st: linux.Statx = undefined;
    if (linux.errno(linux.statx(fd, "", linux.AT.EMPTY_PATH, .{ .TYPE = true, .NLINK = true, .INO = true, .UID = true }, &st)) != .SUCCESS) return null;
    const mode: u32 = st.mode;
    return .{
        .kind = if (linux.S.ISREG(mode)) .regular else if (linux.S.ISDIR(mode)) .directory else .other,
        .dev = makedev(st.dev_major, st.dev_minor),
        .ino = st.ino,
        .nlink = st.nlink,
        .uid = st.uid,
    };
}

/// `F_GETFL` as the kernel's flag word, null on failure.
pub fn getfl(fd: fd_t) ?u32 {
    const rc = linux.fcntl(fd, linux.F.GETFL, 0);
    if (linux.errno(rc) != .SUCCESS) return null;
    return @truncate(rc);
}

/// The open-flag bits the shim screens, as the kernel's words on this
/// target (`linux.O` is a per-arch packed struct).
pub const O = struct {
    pub const ACCMODE: u32 = 0o3;
    pub const RDONLY: u32 = 0;
    pub const WRONLY: u32 = 1;
    pub const RDWR: u32 = 2;
    pub const CREAT: u32 = @bitCast(linux.O{ .CREAT = true });
    /// The kernel's `__O_TMPFILE` bit alone (userspace `O_TMPFILE` ORs
    /// `O_DIRECTORY`, whose value differs per arch; the bit is the screen).
    pub const TMPFILE: u32 = @bitCast(linux.O{ .TMPFILE = true });
    pub const APPEND: u32 = @bitCast(linux.O{ .APPEND = true });
    pub const SYNC: u32 = @bitCast(linux.O{ .SYNC = true });
    pub const DSYNC: u32 = @bitCast(linux.O{ .DSYNC = true });
    pub const PATH: u32 = @bitCast(linux.O{ .PATH = true });
    pub const DIRECT: u32 = @bitCast(linux.O{ .DIRECT = true });
    pub const NOATIME: u32 = @bitCast(linux.O{ .NOATIME = true });
    pub const CLOEXEC: u32 = @bitCast(linux.O{ .CLOEXEC = true });
};

// ---------------------------------------------------------------------------
// Positional / offset syscalls the shim issues itself (never the interposed
// symbol, never dlsym — AS-safe).
// ---------------------------------------------------------------------------

/// `lseek(fd, off, whence)` raw: the kernel's `f_pos` only (no FUSE request
/// for SEEK_SET/SEEK_CUR). Null = the kernel refused (unseekable, EINVAL).
pub fn lseek(fd: fd_t, off: i64, whence: u32) ?u64 {
    const rc = linux.lseek(fd, off, whence);
    if (linux.errno(rc) != .SUCCESS) return null;
    return rc;
}

pub fn lseekRaw(fd: fd_t, off: i64, whence: u32) usize {
    return linux.lseek(fd, off, whence);
}

/// Current `f_pos` (SEEK_CUR 0); null = unseekable.
pub fn tell(fd: fd_t) ?u64 {
    return lseek(fd, 0, linux.SEEK.CUR);
}

/// `SEEK_SET` write-through of a mirror offset; the result is ignored by
/// design (an unseekable description never armed).
pub fn seekSet(fd: fd_t, off: u64) void {
    _ = linux.lseek(fd, @intCast(@min(off, std.math.maxInt(i64))), linux.SEEK.SET);
}

pub fn fgetxattr(fd: fd_t, name: [*:0]const u8, buf: []u8) ?usize {
    const rc = linux.fgetxattr(fd, name, buf.ptr, buf.len);
    if (linux.errno(rc) != .SUCCESS) return null;
    return rc;
}

pub fn close(fd: fd_t) void {
    _ = linux.close(fd);
}

pub fn getpid() u32 {
    return @intCast(linux.getpid());
}

pub fn geteuid() u32 {
    return linux.geteuid();
}

/// Anonymous private memory via the raw syscall (`std.heap.page_allocator`
/// would call libc `mmap` — the shim's own export).
pub fn mapAnon(len: usize) ?[*]align(4096) u8 {
    const rc = linux.mmap(null, len, .{ .READ = true, .WRITE = true }, .{ .TYPE = .PRIVATE, .ANONYMOUS = true }, -1, 0);
    if (linux.errno(rc) != .SUCCESS) return null;
    return @ptrFromInt(rc);
}

pub fn unmapAnon(p: [*]align(4096) u8, len: usize) void {
    _ = linux.munmap(p, len);
}

/// The kernel's `copy_from_user` restated in userspace: `len` bytes at the
/// caller's address `src` into `dst` through `process_vm_readv` on this
/// process, so an unmapped address answers EFAULT where a plain load would
/// fault the app. `.SUCCESS` only when every byte moved.
pub fn copyFromUser(dst: []u8, src: usize) E {
    if (src == 0) return .FAULT;
    const local = [_]std.posix.iovec{.{ .base = dst.ptr, .len = dst.len }};
    const remote = [_]std.posix.iovec_const{.{ .base = @ptrFromInt(src), .len = dst.len }};
    const rc = linux.process_vm_readv(linux.getpid(), &local, &remote, 0);
    return userCopyResult(rc, dst.len);
}

/// `copy_to_user`, the same way (`process_vm_writev`).
pub fn copyToUser(dst: usize, src: []const u8) E {
    const moved = copyToUserPartial(dst, src);
    if (moved.e != .SUCCESS) return moved.e;
    return if (moved.n == src.len) .SUCCESS else .FAULT;
}

/// `copyToUser` that reports how far it got: the bytes moved before an
/// unmapped page (the kernel's per-extent `copy_to_user` leaves the
/// extents before a fault written and counted).
pub fn copyToUserPartial(dst: usize, src: []const u8) struct { e: E, n: usize } {
    if (dst == 0) return .{ .e = .FAULT, .n = 0 };
    if (src.len == 0) return .{ .e = .SUCCESS, .n = 0 };
    const local = [_]std.posix.iovec_const{.{ .base = src.ptr, .len = src.len }};
    const remote = [_]std.posix.iovec_const{.{ .base = @ptrFromInt(dst), .len = src.len }};
    const rc = linux.process_vm_writev(linux.getpid(), &local, &remote, 0);
    const e = linux.errno(rc);
    return if (e != .SUCCESS) .{ .e = e, .n = 0 } else .{ .e = .SUCCESS, .n = rc };
}

fn userCopyResult(rc: usize, want: usize) E {
    const e = linux.errno(rc);
    if (e != .SUCCESS) return e;
    return if (rc == want) .SUCCESS else .FAULT;
}

/// One line to stderr, raw (AS-safe; the panic handler's and the refusal
/// lines' only output path).
pub fn writeStderr(bytes: []const u8) void {
    var off: usize = 0;
    while (off < bytes.len) {
        const rc = linux.write(2, bytes.ptr + off, bytes.len - off);
        if (linux.errno(rc) != .SUCCESS) return;
        if (rc == 0) return;
        off += rc;
    }
}

/// CPUs in this process's affinity mask (what `std.Thread.getCpuCount`
/// reads), without `std.posix`; 1 when the mask is unreadable.
pub fn cpuCount() u32 {
    var set: linux.cpu_set_t = undefined;
    if (linux.errno(linux.sched_getaffinity(0, @sizeOf(linux.cpu_set_t), &set)) != .SUCCESS) return 1;
    var n: u32 = 0;
    for (set) |w| n += @popCount(w);
    return @max(n, 1);
}

/// The process environment as a `knobs`-readable map (the shim shares the
/// ONE knob convention; `std.c.environ` is a data symbol, no call).
pub fn environMap(gpa: std.mem.Allocator) std.mem.Allocator.Error!std.process.Environ.Map {
    var map = std.process.Environ.Map.init(gpa);
    errdefer map.deinit();
    var i: usize = 0;
    while (std.c.environ[i]) |entry| : (i += 1) {
        const s = std.mem.span(entry);
        const eq = std.mem.indexOfScalar(u8, s, '=') orelse continue;
        try map.put(s[0..eq], s[eq + 1 ..]);
    }
    return map;
}

pub fn nanosleep(ns: u64) void {
    const ts: linux.timespec = .{ .sec = @intCast(ns / std.time.ns_per_s), .nsec = @intCast(ns % std.time.ns_per_s) };
    _ = linux.nanosleep(&ts, null);
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the open-flag words match the target's packed layout; FUSE_SUPER_MAGIC and the statfs shape are the kernel's" {
    try testing.expectEqual(@as(u32, 0o100), O.CREAT);
    try testing.expectEqual(@as(u32, 0o2000), O.APPEND);
    try testing.expectEqual(@as(u32, 0o2000000), O.CLOEXEC);
    try testing.expectEqual(@as(u32, 0o10000000), O.PATH);
    try testing.expectEqual(@as(u32, 0o20000000), O.TMPFILE);
    try testing.expectEqual(@as(i64, 0x65735546), FUSE_SUPER_MAGIC);
    try testing.expectEqual(@as(usize, 120), @sizeOf(Statfs));
}

test "fstatfs on /tmp answers a type that is not FUSE; fstat and getfl read a real fd; cpuCount ≥ 1" {
    const fd: fd_t = @intCast(linux.openat(linux.AT.FDCWD, "/tmp", .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .CLOEXEC = true }, 0));
    defer close(fd);
    try testing.expect(fsType(fd).? != FUSE_SUPER_MAGIC);
    const st = fstat(fd).?;
    try testing.expectEqual(FileKind.directory, st.kind);
    try testing.expectEqual(@as(u32, 0), getfl(fd).? & O.ACCMODE);
    try testing.expect(cpuCount() >= 1);
    try testing.expect(fstat(-1) == null);
    try testing.expect(getfl(-1) == null);
}

test "copyFromUser/copyToUser move bytes through this process's own memory and answer EFAULT for NULL, an unmapped page, or a range running into one — never a fault in the caller" {
    var src: [64]u8 = undefined;
    for (&src, 0..) |*b, i| b.* = @intCast(i);
    var dst: [64]u8 = @splat(0);
    try testing.expectEqual(E.SUCCESS, copyFromUser(&dst, @intFromPtr(&src)));
    try testing.expectEqualSlices(u8, &src, &dst);
    @memset(&dst, 0xEE);
    try testing.expectEqual(E.SUCCESS, copyToUser(@intFromPtr(&src), &dst));
    try testing.expectEqualSlices(u8, &dst, &src);
    try testing.expectEqual(E.FAULT, copyFromUser(&dst, 0));
    try testing.expectEqual(E.FAULT, copyToUser(0, &dst));
    // Two pages, the second unmapped: a range across the edge is EFAULT.
    const pages = mapAnon(8192).?;
    unmapAnon(@alignCast(pages + 4096), 4096);
    defer unmapAnon(pages, 4096);
    try testing.expectEqual(E.SUCCESS, copyToUser(@intFromPtr(pages) + 4096 - 32, dst[0..32]));
    try testing.expectEqual(E.FAULT, copyToUser(@intFromPtr(pages) + 4096 - 32, &dst));
    try testing.expectEqual(E.FAULT, copyFromUser(&dst, @intFromPtr(pages) + 4096));
    const part = copyToUserPartial(@intFromPtr(pages) + 4096 - 32, &dst);
    try testing.expectEqual(E.SUCCESS, part.e);
    try testing.expectEqual(@as(usize, 32), part.n);
}

test "libcResult sets errno and answers −1 for a failed raw call; aioResult answers the negative errno" {
    try testing.expectEqual(@as(isize, -1), libcResult(linux.lseek(-1, 0, linux.SEEK.CUR)));
    try testing.expectEqual(E.BADF, getErrno());
    try testing.expectEqual(@as(isize, 7), libcResult(7));
    try testing.expectEqual(-@as(c_int, @backingInt(E.BADF)), aioResult(@bitCast(-@as(isize, @backingInt(E.BADF)))));
    try testing.expectEqual(@as(c_int, 3), aioResult(3));
}
