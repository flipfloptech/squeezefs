//! The interposed symbols (spec `docs/porting/il_shim.md` §1, §6; decision
//! 0002 ledger S3, S10): ONE comptime table generates the exports, the
//! real-symbol resolution, the rails census and the docs table. Every
//! wrapper obeys the plumbing law: resolve the real pointer EAGERLY
//! (`dlsym(RTLD_NEXT)`, cached; `dlvsym` with the default version for
//! libaio), take the guard, run the body (`ops.zig`/`aio.zig`), and call the
//! real function LAZILY — only on a passthrough path. No real symbol ⇒ the
//! kernel's answer is synthesized (the raw syscall where one exists, ENOSYS
//! otherwise). Guard unavailable (a nested entry) ⇒ the real call.
//!
//! The exports exist only in the `.so` (the root declares the build
//! identity): the daemon's test binary links these bodies in linked mode
//! and must never interpose its own I/O.
//!
//! Variadics (S3): `open`/`openat`/`fcntl` are TRUE C-variadic exports where
//! the toolchain emits them (`@cVaStart`); on aarch64 the LLVM backend's
//! `VaList` is disabled, and the AAPCS64 passes a variadic third argument in
//! the same register (x2) as a named one, so the fixed-prototype form reads
//! the identical register. Either form consumes `mode` only under
//! `O_CREAT|O_TMPFILE` and the `fcntl` arg only for commands that take one.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Atomic = std.atomic.Value;

const sys = @import("sys.zig");
const lifecycle = @import("lifecycle.zig");
const classify = @import("classify.zig");
const ops = @import("ops.zig");
const aio = @import("aio.zig");

// ---------------------------------------------------------------------------
// THE table.
// ---------------------------------------------------------------------------

pub const Symbol = struct {
    name: [:0]const u8,
    /// The default symbol version (`dlvsym`); null = plain `dlsym`.
    version: ?[:0]const u8 = null,
    /// The shim's own API (linked mode), not a libc interposition.
    api: bool = false,
};

pub const table = [_]Symbol{
    // positional data
    .{ .name = "pread" },                                                .{ .name = "pread64" },                                              .{ .name = "pwrite" },                                                     .{ .name = "pwrite64" },
    // offsetful data
    .{ .name = "read" },                                                 .{ .name = "write" },
    // vector data
                                                   .{ .name = "readv" },                                                      .{ .name = "writev" },
    .{ .name = "preadv" },                                               .{ .name = "preadv64" },                                             .{ .name = "pwritev" },                                                    .{ .name = "pwritev64" },
    .{ .name = "preadv2" },                                              .{ .name = "preadv64v2" },                                           .{ .name = "pwritev2" },                                                   .{ .name = "pwritev64v2" },
    // lseek
    .{ .name = "lseek" },                                                .{ .name = "lseek64" },
    // f_pos-consuming passthroughs
                                                 .{ .name = "sendfile" },                                                   .{ .name = "sendfile64" },
    .{ .name = "copy_file_range" },                                      .{ .name = "splice" },
    // spawn
                                                  .{ .name = "posix_spawn" },                                                .{ .name = "posix_spawnp" },
    // detection
    .{ .name = "open" },                                                 .{ .name = "open64" },                                               .{ .name = "openat" },                                                     .{ .name = "openat64" },
    .{ .name = "creat" },                                                .{ .name = "creat64" },                                              .{ .name = "openat2" },
    // lifecycle
                                                       .{ .name = "close" },
    .{ .name = "close_range" },                                          .{ .name = "dup" },                                                  .{ .name = "dup2" },                                                       .{ .name = "dup3" },
    .{ .name = "fcntl" },                                                .{ .name = "fcntl64" },
    // mmap
                                                 .{ .name = "mmap" },                                                       .{ .name = "mmap64" },
    // fd-creator hygiene
    .{ .name = "socket" },                                               .{ .name = "eventfd" },                                              .{ .name = "epoll_create" },                                               .{ .name = "epoll_create1" },
    .{ .name = "timerfd_create" },                                       .{ .name = "inotify_init" },                                         .{ .name = "inotify_init1" },                                              .{ .name = "memfd_create" },
    .{ .name = "accept" },                                               .{ .name = "accept4" },                                              .{ .name = "signalfd" },                                                   .{ .name = "signalfd4" },
    .{ .name = "pipe" },                                                 .{ .name = "pipe2" },                                                .{ .name = "socketpair" },
    // libaio
                                                    .{ .name = "io_setup", .version = aio.Versions.io_setup },
    .{ .name = "io_destroy", .version = aio.Versions.io_destroy },       .{ .name = "io_submit", .version = aio.Versions.io_submit },         .{ .name = "io_getevents", .version = aio.Versions.io_getevents },         .{ .name = "io_pgetevents", .version = aio.Versions.io_pgetevents },
    .{ .name = "io_cancel", .version = aio.Versions.io_cancel },
    // libaio's queue face (composed from the rows above, never resolved)
            .{ .name = "io_queue_init", .version = aio.Versions.io_queue_init }, .{ .name = "io_queue_release", .version = aio.Versions.io_queue_release }, .{ .name = "io_queue_run", .version = aio.Versions.io_queue_run },
    .{ .name = "io_queue_wait", .version = aio.Versions.io_queue_wait },
    // the linked-mode API
    .{ .name = "squeezefs_il_bind", .api = true },
};

/// The libc/libaio interpositions: the spec's 59 + `openat2` + `signalfd4`
/// + libaio's four-symbol queue face (`io_queue_init/release/run/wait`,
/// which libaio binds to its OWN `io_setup`/`io_getevents` internally —
/// fio's libaio engine created its context through it uncounted).
pub const INTERPOSED: usize = 65;

comptime {
    var n: usize = 0;
    for (table) |e| {
        if (!e.api) n += 1;
    }
    std.debug.assert(n == INTERPOSED);
}

pub fn index(comptime name: []const u8) usize {
    inline for (table, 0..) |e, i| if (comptime std.mem.eql(u8, e.name, name)) return i;
    @compileError("not an interposed symbol: " ++ name);
}

// ---------------------------------------------------------------------------
// Real-symbol resolution: once per symbol, null cached too.
// ---------------------------------------------------------------------------

const UNRESOLVED: u8 = 0;
const MISSING: u8 = 1;
const RESOLVED: u8 = 2;

var real_ptrs: [table.len]Atomic(?*anyopaque) = @splat(.init(null));
var real_state: [table.len]Atomic(u8) = @splat(.init(UNRESOLVED));

fn RealFn(comptime name: []const u8) type {
    return *const @TypeOf(@field(wrappers, name));
}

/// The next definition of `name` after the shim in the link map.
pub fn real(comptime name: []const u8) ?RealFn(name) {
    const i = comptime index(name);
    switch (real_state[i].load(.acquire)) {
        RESOLVED => return @ptrCast(@alignCast(real_ptrs[i].load(.acquire))),
        MISSING => return null,
        else => {},
    }
    const e = table[i];
    var p: ?*anyopaque = null;
    if (e.version) |v| p = sys.dlvsym(sys.RTLD_NEXT, e.name, v);
    if (p == null) p = sys.dlsym(sys.RTLD_NEXT, e.name);
    real_ptrs[i].store(p, .release);
    real_state[i].store(if (p == null) MISSING else RESOLVED, .release);
    return @ptrCast(@alignCast(p));
}

/// Was the real symbol resolvable? (tests; the census line)
pub fn realResolved(comptime name: []const u8) bool {
    return real(name) != null;
}

fn enosys() isize {
    sys.setErrno(.NOSYS);
    return -1;
}

fn enosysInt() c_int {
    sys.setErrno(.NOSYS);
    return -1;
}

/// The state for a data op: never initializes (no fd is bound before the
/// first `open` built it).
inline fn ready() ?*classify.State {
    return lifecycle.stateIfReady();
}

const off_t = i64;
const Iovec = ops.Iovec;

/// On x86_64 the variadic forms are emitted; on aarch64 under LLVM the
/// fixed-register form (module doc).
const va_ok = !(builtin.cpu.arch == .aarch64 and builtin.zig_backend == .stage2_llvm);

// ---------------------------------------------------------------------------
// The wrappers: one per table row, named exactly as the symbol.
// ---------------------------------------------------------------------------

pub const wrappers = struct {
    // ---- positional ----

    fn preadBody(comptime name: []const u8, op: ops.Op, fd: c_int, buf: ?[*]u8, n: usize, off: off_t) isize {
        const r = real(name);
        if (!lifecycle.enter()) {
            classify.note(.reentrant, null);
            return if (r) |f| f(fd, buf, n, off) else enosys();
        }
        defer lifecycle.leave();
        if (ready()) |st| switch (ops.positional(st, op, fd, buf, n, off)) {
            .done => |v| return v,
            .real => {},
        };
        if (r) |f| return f(fd, buf, n, off);
        classify.note(.no_real_symbol, null);
        return sys.libcResult(if (op == .read) linux.pread(fd, buf orelse return enosys(), n, off) else linux.pwrite(fd, buf orelse return enosys(), n, off));
    }

    pub fn pread(fd: c_int, buf: ?[*]u8, n: usize, off: off_t) callconv(.c) isize {
        return preadBody("pread", .read, fd, buf, n, off);
    }
    pub fn pread64(fd: c_int, buf: ?[*]u8, n: usize, off: off_t) callconv(.c) isize {
        return preadBody("pread64", .read, fd, buf, n, off);
    }
    pub fn pwrite(fd: c_int, buf: ?[*]u8, n: usize, off: off_t) callconv(.c) isize {
        return preadBody("pwrite", .write, fd, buf, n, off);
    }
    pub fn pwrite64(fd: c_int, buf: ?[*]u8, n: usize, off: off_t) callconv(.c) isize {
        return preadBody("pwrite64", .write, fd, buf, n, off);
    }

    // ---- offsetful ----

    fn readBody(comptime name: []const u8, op: ops.Op, fd: c_int, buf: ?[*]u8, n: usize) isize {
        const r = real(name);
        if (!lifecycle.enter()) {
            classify.note(.reentrant, null);
            return if (r) |f| f(fd, buf, n) else enosys();
        }
        defer lifecycle.leave();
        if (ready()) |st| switch (ops.offsetful(st, op, fd, buf, n)) {
            .done => |v| return v,
            .real => {},
        };
        if (r) |f| return f(fd, buf, n);
        classify.note(.no_real_symbol, null);
        return sys.libcResult(if (op == .read) linux.read(fd, buf orelse return enosys(), n) else linux.write(fd, buf orelse return enosys(), n));
    }

    pub fn read(fd: c_int, buf: ?[*]u8, n: usize) callconv(.c) isize {
        return readBody("read", .read, fd, buf, n);
    }
    pub fn write(fd: c_int, buf: ?[*]u8, n: usize) callconv(.c) isize {
        return readBody("write", .write, fd, buf, n);
    }

    // ---- vector ----

    fn vecBody(comptime name: []const u8, op: ops.Op, fd: c_int, iov: ?[*]const Iovec, cnt: c_int, off: ?off_t, rwf: u32, comptime call: anytype) isize {
        const r = real(name);
        if (!lifecycle.enter()) {
            classify.note(.reentrant, null);
            return call(r, fd, iov, cnt, off, rwf);
        }
        defer lifecycle.leave();
        if (ready()) |st| switch (ops.vector(st, op, fd, iov, cnt, off, rwf)) {
            .done => |v| return v,
            .real => {},
        };
        return call(r, fd, iov, cnt, off, rwf);
    }

    fn callV(r: anytype, fd: c_int, iov: ?[*]const Iovec, cnt: c_int, _: ?off_t, _: u32) isize {
        if (r) |f| return f(fd, iov, cnt);
        return enosys();
    }
    fn callPV(r: anytype, fd: c_int, iov: ?[*]const Iovec, cnt: c_int, off: ?off_t, _: u32) isize {
        if (r) |f| return f(fd, iov, cnt, off orelse -1);
        return enosys();
    }
    fn callPV2(r: anytype, fd: c_int, iov: ?[*]const Iovec, cnt: c_int, off: ?off_t, rwf: u32) isize {
        if (r) |f| return f(fd, iov, cnt, off orelse -1, @bitCast(rwf));
        return enosys();
    }

    pub fn readv(fd: c_int, iov: ?[*]const Iovec, cnt: c_int) callconv(.c) isize {
        return vecBody("readv", .read, fd, iov, cnt, null, 0, callV);
    }
    pub fn writev(fd: c_int, iov: ?[*]const Iovec, cnt: c_int) callconv(.c) isize {
        return vecBody("writev", .write, fd, iov, cnt, null, 0, callV);
    }
    pub fn preadv(fd: c_int, iov: ?[*]const Iovec, cnt: c_int, off: off_t) callconv(.c) isize {
        return vecBody("preadv", .read, fd, iov, cnt, off, 0, callPV);
    }
    pub fn preadv64(fd: c_int, iov: ?[*]const Iovec, cnt: c_int, off: off_t) callconv(.c) isize {
        return vecBody("preadv64", .read, fd, iov, cnt, off, 0, callPV);
    }
    pub fn pwritev(fd: c_int, iov: ?[*]const Iovec, cnt: c_int, off: off_t) callconv(.c) isize {
        return vecBody("pwritev", .write, fd, iov, cnt, off, 0, callPV);
    }
    pub fn pwritev64(fd: c_int, iov: ?[*]const Iovec, cnt: c_int, off: off_t) callconv(.c) isize {
        return vecBody("pwritev64", .write, fd, iov, cnt, off, 0, callPV);
    }
    /// `offset == -1` is the offsetful shape (the kernel's `f_pos`).
    pub fn preadv2(fd: c_int, iov: ?[*]const Iovec, cnt: c_int, off: off_t, flags: c_int) callconv(.c) isize {
        return vecBody("preadv2", .read, fd, iov, cnt, if (off == -1) null else off, @bitCast(flags), callPV2);
    }
    pub fn preadv64v2(fd: c_int, iov: ?[*]const Iovec, cnt: c_int, off: off_t, flags: c_int) callconv(.c) isize {
        return vecBody("preadv64v2", .read, fd, iov, cnt, if (off == -1) null else off, @bitCast(flags), callPV2);
    }
    pub fn pwritev2(fd: c_int, iov: ?[*]const Iovec, cnt: c_int, off: off_t, flags: c_int) callconv(.c) isize {
        return vecBody("pwritev2", .write, fd, iov, cnt, if (off == -1) null else off, @bitCast(flags), callPV2);
    }
    pub fn pwritev64v2(fd: c_int, iov: ?[*]const Iovec, cnt: c_int, off: off_t, flags: c_int) callconv(.c) isize {
        return vecBody("pwritev64v2", .write, fd, iov, cnt, if (off == -1) null else off, @bitCast(flags), callPV2);
    }

    // ---- lseek ----

    fn lseekBody(comptime name: []const u8, fd: c_int, off: off_t, whence: c_int) off_t {
        const r = real(name);
        if (!lifecycle.enter()) {
            classify.note(.reentrant, null);
            return if (r) |f| f(fd, off, whence) else sys.libcResult(sys.lseekRaw(fd, off, @bitCast(whence)));
        }
        defer lifecycle.leave();
        if (ready()) |st| switch (ops.lseek(st, fd, off, whence)) {
            .done => |v| return v,
            .real => {},
        };
        if (r) |f| return f(fd, off, whence);
        classify.note(.no_real_symbol, null);
        return sys.libcResult(sys.lseekRaw(fd, off, @bitCast(whence)));
    }

    pub fn lseek(fd: c_int, off: off_t, whence: c_int) callconv(.c) off_t {
        return lseekBody("lseek", fd, off, whence);
    }
    pub fn lseek64(fd: c_int, off: off_t, whence: c_int) callconv(.c) off_t {
        return lseekBody("lseek64", fd, off, whence);
    }

    // ---- f_pos-consuming passthroughs: the real call ALWAYS executes ----

    fn sendfileBody(comptime name: []const u8, out_fd: c_int, in_fd: c_int, off: ?*off_t, count: usize) isize {
        const r = real(name);
        if (lifecycle.enter()) {
            defer lifecycle.leave();
            if (ready()) |st| ops.sendfilePre(st, out_fd, in_fd, off == null);
        }
        if (r) |f| return f(out_fd, in_fd, off, count);
        return sys.libcResult(linux.sendfile(out_fd, in_fd, off, count));
    }

    pub fn sendfile(out_fd: c_int, in_fd: c_int, off: ?*off_t, count: usize) callconv(.c) isize {
        return sendfileBody("sendfile", out_fd, in_fd, off, count);
    }
    pub fn sendfile64(out_fd: c_int, in_fd: c_int, off: ?*off_t, count: usize) callconv(.c) isize {
        return sendfileBody("sendfile64", out_fd, in_fd, off, count);
    }

    pub fn copy_file_range(fd_in: c_int, off_in: ?*off_t, fd_out: c_int, off_out: ?*off_t, len: usize, flags: c_uint) callconv(.c) isize {
        const r = real("copy_file_range");
        if (lifecycle.enter()) {
            defer lifecycle.leave();
            if (ready()) |st| ops.copyRangePre(st, fd_in, off_in == null, fd_out, off_out == null);
        }
        if (r) |f| return f(fd_in, off_in, fd_out, off_out, len, flags);
        return sys.libcResult(linux.copy_file_range(fd_in, off_in, fd_out, off_out, len, flags));
    }

    pub fn splice(fd_in: c_int, off_in: ?*off_t, fd_out: c_int, off_out: ?*off_t, len: usize, flags: c_uint) callconv(.c) isize {
        const r = real("splice");
        if (lifecycle.enter()) {
            defer lifecycle.leave();
            if (ready()) |st| ops.copyRangePre(st, fd_in, off_in == null, fd_out, off_out == null);
        }
        if (r) |f| return f(fd_in, off_in, fd_out, off_out, len, flags);
        return sys.libcResult(linux.syscall6(.splice, @as(u32, @bitCast(fd_in)), @intFromPtr(off_in), @as(u32, @bitCast(fd_out)), @intFromPtr(off_out), len, flags));
    }

    // ---- posix_spawn: the fork flush, then the real spawn ----

    fn spawnBody(comptime name: []const u8, pid: ?*linux.pid_t, path: ?[*:0]const u8, fa: ?*const anyopaque, attr: ?*const anyopaque, argv: ?[*:null]const ?[*:0]const u8, envp: ?[*:null]const ?[*:0]const u8) c_int {
        const r = real(name);
        if (lifecycle.enter()) {
            defer lifecycle.leave();
            if (ready()) |st| ops.spawnPre(st);
        }
        if (r) |f| return f(pid, path, fa, attr, argv, envp);
        return @backingInt(linux.E.NOSYS);
    }

    pub fn posix_spawn(pid: ?*linux.pid_t, path: ?[*:0]const u8, fa: ?*const anyopaque, attr: ?*const anyopaque, argv: ?[*:null]const ?[*:0]const u8, envp: ?[*:null]const ?[*:0]const u8) callconv(.c) c_int {
        return spawnBody("posix_spawn", pid, path, fa, attr, argv, envp);
    }
    pub fn posix_spawnp(pid: ?*linux.pid_t, path: ?[*:0]const u8, fa: ?*const anyopaque, attr: ?*const anyopaque, argv: ?[*:null]const ?[*:0]const u8, envp: ?[*:null]const ?[*:0]const u8) callconv(.c) c_int {
        return spawnBody("posix_spawnp", pid, path, fa, attr, argv, envp);
    }

    // ---- detection: the open family ----

    /// The fork-epoch snapshot BEFORE the real open (`maxInt` = never arm:
    /// the state could not be built).
    fn epochSnapshot() u64 {
        const st = lifecycle.state() orelse return std.math.maxInt(u64);
        return st.table.currentEpoch();
    }

    /// After the real call: classify under the guard. The fd is returned
    /// regardless of any bookkeeping outcome.
    fn afterOpen(fd: c_int, epoch: u64) c_int {
        if (fd < 0) return fd;
        if (!lifecycle.enter()) return fd;
        defer lifecycle.leave();
        if (lifecycle.stateIfReady()) |st| _ = classify.classifyAndBind(st, classify.Probes.real, fd, epoch);
        return fd;
    }

    fn modeWanted(flags: c_int) bool {
        const f: u32 = @bitCast(flags);
        return f & sys.O.CREAT != 0 or f & sys.O.TMPFILE != 0;
    }

    fn openBody(comptime name: []const u8, path: ?[*:0]const u8, flags: c_int, mode: c_uint, mode_known: bool) c_int {
        const r = real(name);
        const epoch = epochSnapshot();
        const fd = if (r) |f| callOpen(f, .{ path, flags }, mode, mode_known) else blk: {
            classify.note(.no_real_symbol, null);
            const p = path orelse return enosysInt();
            break :blk @as(c_int, @intCast(sys.libcResult(linux.openat(linux.AT.FDCWD, p, @bitCast(@as(u32, @bitCast(flags))), if (mode_known) mode else 0))));
        };
        return afterOpen(fd, epoch);
    }

    /// The variadic real `open` is called with `mode` only when it is
    /// meaningful; the fixed-prototype form always passes the register.
    fn callOpen(f: anytype, args: anytype, mode: c_uint, mode_known: bool) c_int {
        if (va_ok) {
            if (mode_known) return @call(.auto, f, args ++ .{mode});
            return @call(.auto, f, args);
        }
        return @call(.auto, f, args ++ .{mode});
    }

    fn openatBody(comptime name: []const u8, dirfd: c_int, path: ?[*:0]const u8, flags: c_int, mode: c_uint, mode_known: bool) c_int {
        const r = real(name);
        const epoch = epochSnapshot();
        const fd = if (r) |f| callOpen(f, .{ dirfd, path, flags }, mode, mode_known) else blk: {
            classify.note(.no_real_symbol, null);
            const p = path orelse return enosysInt();
            break :blk @as(c_int, @intCast(sys.libcResult(linux.openat(dirfd, p, @bitCast(@as(u32, @bitCast(flags))), if (mode_known) mode else 0))));
        };
        return afterOpen(fd, epoch);
    }

    // The variadic forms (x86_64): `mode` is read only under O_CREAT|O_TMPFILE.
    pub const open = if (va_ok) openVa else openFixed;
    pub const open64 = if (va_ok) open64Va else open64Fixed;
    pub const openat = if (va_ok) openatVa else openatFixed;
    pub const openat64 = if (va_ok) openat64Va else openat64Fixed;

    fn openVa(path: ?[*:0]const u8, flags: c_int, ...) callconv(.c) c_int {
        var ap = @cVaStart();
        defer @cVaEnd(&ap);
        const wanted = modeWanted(flags);
        const mode: c_uint = if (wanted) @cVaArg(&ap, c_uint) else 0;
        return openBody("open", path, flags, mode, wanted);
    }
    fn open64Va(path: ?[*:0]const u8, flags: c_int, ...) callconv(.c) c_int {
        var ap = @cVaStart();
        defer @cVaEnd(&ap);
        const wanted = modeWanted(flags);
        const mode: c_uint = if (wanted) @cVaArg(&ap, c_uint) else 0;
        return openBody("open64", path, flags, mode, wanted);
    }
    fn openatVa(dirfd: c_int, path: ?[*:0]const u8, flags: c_int, ...) callconv(.c) c_int {
        var ap = @cVaStart();
        defer @cVaEnd(&ap);
        const wanted = modeWanted(flags);
        const mode: c_uint = if (wanted) @cVaArg(&ap, c_uint) else 0;
        return openatBody("openat", dirfd, path, flags, mode, wanted);
    }
    fn openat64Va(dirfd: c_int, path: ?[*:0]const u8, flags: c_int, ...) callconv(.c) c_int {
        var ap = @cVaStart();
        defer @cVaEnd(&ap);
        const wanted = modeWanted(flags);
        const mode: c_uint = if (wanted) @cVaArg(&ap, c_uint) else 0;
        return openatBody("openat64", dirfd, path, flags, mode, wanted);
    }
    // The fixed-register forms (aarch64 under LLVM): the third argument
    // register is consumed only under O_CREAT|O_TMPFILE.
    fn openFixed(path: ?[*:0]const u8, flags: c_int, mode_reg: c_uint) callconv(.c) c_int {
        const wanted = modeWanted(flags);
        return openBody("open", path, flags, if (wanted) mode_reg else 0, wanted);
    }
    fn open64Fixed(path: ?[*:0]const u8, flags: c_int, mode_reg: c_uint) callconv(.c) c_int {
        const wanted = modeWanted(flags);
        return openBody("open64", path, flags, if (wanted) mode_reg else 0, wanted);
    }
    fn openatFixed(dirfd: c_int, path: ?[*:0]const u8, flags: c_int, mode_reg: c_uint) callconv(.c) c_int {
        const wanted = modeWanted(flags);
        return openatBody("openat", dirfd, path, flags, if (wanted) mode_reg else 0, wanted);
    }
    fn openat64Fixed(dirfd: c_int, path: ?[*:0]const u8, flags: c_int, mode_reg: c_uint) callconv(.c) c_int {
        const wanted = modeWanted(flags);
        return openatBody("openat64", dirfd, path, flags, if (wanted) mode_reg else 0, wanted);
    }

    fn creatBody(comptime name: []const u8, path: ?[*:0]const u8, mode: c_uint) c_int {
        const r = real(name);
        const epoch = epochSnapshot();
        const fd = if (r) |f| f(path, mode) else blk: {
            classify.note(.no_real_symbol, null);
            const p = path orelse return enosysInt();
            break :blk @as(c_int, @intCast(sys.libcResult(linux.openat(linux.AT.FDCWD, p, .{ .ACCMODE = .WRONLY, .CREAT = true, .TRUNC = true }, mode))));
        };
        return afterOpen(fd, epoch);
    }

    pub fn creat(path: ?[*:0]const u8, mode: c_uint) callconv(.c) c_int {
        return creatBody("creat", path, mode);
    }
    pub fn creat64(path: ?[*:0]const u8, mode: c_uint) callconv(.c) c_int {
        return creatBody("creat64", path, mode);
    }

    /// glibc exports no `openat2`: the raw syscall is the usual real call.
    pub fn openat2(dirfd: c_int, path: ?[*:0]const u8, how: ?*anyopaque, size: usize) callconv(.c) c_int {
        const r = real("openat2");
        const epoch = epochSnapshot();
        const fd = if (r) |f| f(dirfd, path, how, size) else @as(c_int, @intCast(sys.libcResult(linux.syscall4(.openat2, @as(u32, @bitCast(dirfd)), @intFromPtr(path), @intFromPtr(how), size))));
        return afterOpen(fd, epoch);
    }

    // ---- lifecycle ----

    pub fn close(fd: c_int) callconv(.c) c_int {
        const r = real("close");
        if (lifecycle.enter()) {
            defer lifecycle.leave();
            if (ready()) |st| ops.closePre(st, fd);
        }
        if (r) |f| return f(fd);
        return @intCast(sys.libcResult(linux.close(fd)));
    }

    pub fn close_range(first: c_uint, last: c_uint, flags: c_int) callconv(.c) c_int {
        const r = real("close_range");
        if (lifecycle.enter()) {
            defer lifecycle.leave();
            if (ready()) |st| ops.closeRangePre(st, first, last, @bitCast(flags));
        }
        if (r) |f| return f(first, last, flags);
        return @intCast(sys.libcResult(linux.close_range(@bitCast(first), @bitCast(last), @bitCast(@as(u32, @bitCast(flags))))));
    }

    fn afterDup(oldfd: c_int, newfd: c_int) c_int {
        if (newfd < 0 or oldfd == newfd) return newfd;
        if (!lifecycle.enter()) return newfd;
        defer lifecycle.leave();
        if (ready()) |st| ops.dupPost(st, oldfd, newfd);
        return newfd;
    }

    pub fn dup(fd: c_int) callconv(.c) c_int {
        const r = real("dup");
        const n = if (r) |f| f(fd) else @as(c_int, @intCast(sys.libcResult(linux.dup(fd))));
        return afterDup(fd, n);
    }
    pub fn dup2(oldfd: c_int, newfd: c_int) callconv(.c) c_int {
        const r = real("dup2");
        const n = if (r) |f| f(oldfd, newfd) else @as(c_int, @intCast(sys.libcResult(linux.dup2(oldfd, newfd))));
        return afterDup(oldfd, n);
    }
    pub fn dup3(oldfd: c_int, newfd: c_int, flags: c_int) callconv(.c) c_int {
        const r = real("dup3");
        const n = if (r) |f| f(oldfd, newfd, flags) else @as(c_int, @intCast(sys.libcResult(linux.dup3(oldfd, newfd, @bitCast(flags)))));
        return afterDup(oldfd, n);
    }

    /// Commands that take no third argument: the register is never read.
    fn fcntlHasArg(cmd: c_int) bool {
        const F = linux.F;
        return switch (cmd) {
            F.GETFD, F.GETFL, F.GETOWN, F.GETSIG, F.GETLEASE, F.GETPIPE_SZ, F.GET_SEALS => false,
            else => true,
        };
    }

    fn fcntlBody(comptime name: []const u8, fd: c_int, cmd: c_int, arg: usize) c_int {
        const r = real(name);
        if (lifecycle.enter()) {
            defer lifecycle.leave();
            if (ready()) |st| ops.fcntlPre(st, fd, cmd, arg);
        }
        const rc = if (r) |f| f(fd, cmd, arg) else @as(c_int, @intCast(sys.libcResult(linux.fcntl(fd, cmd, arg))));
        if (cmd == linux.F.DUPFD or cmd == linux.F.DUPFD_CLOEXEC) return afterDup(fd, rc);
        return rc;
    }

    pub const fcntl = if (va_ok) fcntlVa else fcntlFixed;
    pub const fcntl64 = if (va_ok) fcntl64Va else fcntl64Fixed;

    fn fcntlVa(fd: c_int, cmd: c_int, ...) callconv(.c) c_int {
        var ap = @cVaStart();
        defer @cVaEnd(&ap);
        const arg: usize = if (fcntlHasArg(cmd)) @cVaArg(&ap, usize) else 0;
        return fcntlBody("fcntl", fd, cmd, arg);
    }
    fn fcntl64Va(fd: c_int, cmd: c_int, ...) callconv(.c) c_int {
        var ap = @cVaStart();
        defer @cVaEnd(&ap);
        const arg: usize = if (fcntlHasArg(cmd)) @cVaArg(&ap, usize) else 0;
        return fcntlBody("fcntl64", fd, cmd, arg);
    }
    fn fcntlFixed(fd: c_int, cmd: c_int, arg_reg: usize) callconv(.c) c_int {
        return fcntlBody("fcntl", fd, cmd, if (fcntlHasArg(cmd)) arg_reg else 0);
    }
    fn fcntl64Fixed(fd: c_int, cmd: c_int, arg_reg: usize) callconv(.c) c_int {
        return fcntlBody("fcntl64", fd, cmd, if (fcntlHasArg(cmd)) arg_reg else 0);
    }

    // ---- mmap ----

    const MAP_FAILED: ?*anyopaque = @ptrFromInt(std.math.maxInt(usize));

    fn mmapBody(comptime name: []const u8, addr: ?*anyopaque, len: usize, prot: c_int, flags: c_int, fd: c_int, off: off_t) ?*anyopaque {
        const r = real(name);
        if (lifecycle.enter()) {
            defer lifecycle.leave();
            if (ready()) |st| ops.mmapPre(st, fd, flags);
        }
        if (r) |f| return f(addr, len, prot, flags, fd, off);
        const rc = linux.mmap(@ptrCast(addr), len, @bitCast(@as(u32, @bitCast(prot))), @bitCast(@as(u32, @bitCast(flags))), fd, off);
        if (linux.errno(rc) != .SUCCESS) {
            sys.setErrno(linux.errno(rc));
            return MAP_FAILED;
        }
        return @ptrFromInt(rc);
    }

    pub fn mmap(addr: ?*anyopaque, len: usize, prot: c_int, flags: c_int, fd: c_int, off: off_t) callconv(.c) ?*anyopaque {
        return mmapBody("mmap", addr, len, prot, flags, fd, off);
    }
    pub fn mmap64(addr: ?*anyopaque, len: usize, prot: c_int, flags: c_int, fd: c_int, off: off_t) callconv(.c) ?*anyopaque {
        return mmapBody("mmap64", addr, len, prot, flags, fd, off);
    }

    // ---- fd-creator hygiene: the real call, then the sweep ----

    fn swept(fd: c_int) c_int {
        if (fd < 0) return fd;
        if (!lifecycle.enter()) return fd;
        defer lifecycle.leave();
        if (ready()) |st| ops.sweep(st, fd);
        return fd;
    }

    fn sweptPair(rc: c_int, fds: ?*[2]c_int) c_int {
        if (rc != 0) return rc;
        const p = fds orelse return rc;
        if (!lifecycle.enter()) return rc;
        defer lifecycle.leave();
        if (ready()) |st| {
            ops.sweep(st, p[0]);
            ops.sweep(st, p[1]);
        }
        return rc;
    }

    pub fn socket(domain: c_int, kind: c_int, protocol: c_int) callconv(.c) c_int {
        const r = real("socket");
        return swept(if (r) |f| f(domain, kind, protocol) else @as(c_int, @intCast(sys.libcResult(linux.socket(@bitCast(domain), @bitCast(kind), @bitCast(protocol))))));
    }
    pub fn eventfd(initval: c_uint, flags: c_int) callconv(.c) c_int {
        const r = real("eventfd");
        return swept(if (r) |f| f(initval, flags) else @as(c_int, @intCast(sys.libcResult(linux.eventfd(initval, @bitCast(flags))))));
    }
    pub fn epoll_create(size: c_int) callconv(.c) c_int {
        const r = real("epoll_create");
        return swept(if (r) |f| f(size) else @as(c_int, @intCast(sys.libcResult(linux.epoll_create1(0)))));
    }
    pub fn epoll_create1(flags: c_int) callconv(.c) c_int {
        const r = real("epoll_create1");
        return swept(if (r) |f| f(flags) else @as(c_int, @intCast(sys.libcResult(linux.epoll_create1(@bitCast(flags))))));
    }
    pub fn timerfd_create(clockid: c_int, flags: c_int) callconv(.c) c_int {
        const r = real("timerfd_create");
        return swept(if (r) |f| f(clockid, flags) else @as(c_int, @intCast(sys.libcResult(linux.syscall2(.timerfd_create, @as(u32, @bitCast(clockid)), @as(u32, @bitCast(flags)))))));
    }
    pub fn inotify_init() callconv(.c) c_int {
        const r = real("inotify_init");
        return swept(if (r) |f| f() else @as(c_int, @intCast(sys.libcResult(linux.inotify_init1(0)))));
    }
    pub fn inotify_init1(flags: c_int) callconv(.c) c_int {
        const r = real("inotify_init1");
        return swept(if (r) |f| f(flags) else @as(c_int, @intCast(sys.libcResult(linux.inotify_init1(@bitCast(flags))))));
    }
    pub fn memfd_create(name: ?[*:0]const u8, flags: c_uint) callconv(.c) c_int {
        const r = real("memfd_create");
        return swept(if (r) |f| f(name, flags) else @as(c_int, @intCast(sys.libcResult(linux.memfd_create(name orelse return enosysInt(), flags)))));
    }
    pub fn accept(fd: c_int, addr: ?*linux.sockaddr, len: ?*u32) callconv(.c) c_int {
        const r = real("accept");
        return swept(if (r) |f| f(fd, addr, len) else @as(c_int, @intCast(sys.libcResult(linux.accept4(fd, addr, len, 0)))));
    }
    pub fn accept4(fd: c_int, addr: ?*linux.sockaddr, len: ?*u32, flags: c_int) callconv(.c) c_int {
        const r = real("accept4");
        return swept(if (r) |f| f(fd, addr, len, flags) else @as(c_int, @intCast(sys.libcResult(linux.accept4(fd, addr, len, @bitCast(flags))))));
    }
    pub fn signalfd(fd: c_int, mask: ?*const anyopaque, flags: c_int) callconv(.c) c_int {
        const r = real("signalfd");
        return swept(if (r) |f| f(fd, mask, flags) else @as(c_int, @intCast(sys.libcResult(linux.syscall4(.signalfd4, @as(u32, @bitCast(fd)), @intFromPtr(mask), 8, @as(u32, @bitCast(flags)))))));
    }
    /// glibc exports no `signalfd4`: the raw syscall is the usual real call.
    pub fn signalfd4(fd: c_int, mask: ?*const anyopaque, sizemask: usize, flags: c_int) callconv(.c) c_int {
        const r = real("signalfd4");
        return swept(if (r) |f| f(fd, mask, sizemask, flags) else @as(c_int, @intCast(sys.libcResult(linux.syscall4(.signalfd4, @as(u32, @bitCast(fd)), @intFromPtr(mask), sizemask, @as(u32, @bitCast(flags)))))));
    }
    pub fn pipe(fds: ?*[2]c_int) callconv(.c) c_int {
        const r = real("pipe");
        return sweptPair(if (r) |f| f(fds) else @as(c_int, @intCast(sys.libcResult(linux.pipe2(fds orelse return enosysInt(), .{})))), fds);
    }
    pub fn pipe2(fds: ?*[2]c_int, flags: c_int) callconv(.c) c_int {
        const r = real("pipe2");
        return sweptPair(if (r) |f| f(fds, flags) else @as(c_int, @intCast(sys.libcResult(linux.pipe2(fds orelse return enosysInt(), @bitCast(@as(u32, @bitCast(flags))))))), fds);
    }
    pub fn socketpair(domain: c_int, kind: c_int, protocol: c_int, sv: ?*[2]c_int) callconv(.c) c_int {
        const r = real("socketpair");
        return sweptPair(if (r) |f| f(domain, kind, protocol, sv) else @as(c_int, @intCast(sys.libcResult(linux.socketpair(@bitCast(domain), @bitCast(kind), @bitCast(protocol), sv orelse return enosysInt())))), sv);
    }

    // ---- libaio (the real symbols by version; raw syscalls when absent:
    //      libaio's functions are the kernel's with its −errno convention) ----

    fn aioFns() aio.RealFns {
        return .{ .io_submit = real("io_submit"), .io_getevents = real("io_getevents") };
    }

    pub fn io_setup(nr: c_int, ctxp: ?*aio.io_context_t) callconv(.c) c_int {
        const r = real("io_setup");
        const p = ctxp orelse return -@as(c_int, @backingInt(linux.E.INVAL));
        const rc = if (r) |f| f(nr, p) else sys.aioResult(linux.syscall2(.io_setup, @as(u32, @bitCast(nr)), @intFromPtr(p)));
        if (rc == 0 and lifecycle.enter()) {
            defer lifecycle.leave();
            if (lifecycle.state()) |st| aio.setupPost(st, nr, p, aioFns());
        }
        return rc;
    }

    pub fn io_destroy(ctx: aio.io_context_t) callconv(.c) c_int {
        const r = real("io_destroy");
        // Bookkeeping FIRST and unconditionally (a skipped vacate is the
        // field crash class).
        aio.destroyPre(ready(), ctx, aioFns());
        if (r) |f| return f(ctx);
        return sys.aioResult(linux.syscall1(.io_destroy, ctx));
    }

    pub fn io_submit(ctx: aio.io_context_t, nr: c_long, ios: ?[*]const ?*aio.Iocb) callconv(.c) c_int {
        const r = real("io_submit");
        if (lifecycle.enter()) {
            defer lifecycle.leave();
            if (ready()) |st| switch (aio.submit(st, ctx, nr, ios, aioFns())) {
                .done => |v| return v,
                .real => {},
            };
        }
        if (r) |f| return f(ctx, nr, ios);
        return sys.aioResult(linux.syscall3(.io_submit, ctx, @bitCast(nr), @intFromPtr(ios)));
    }

    pub fn io_getevents(ctx: aio.io_context_t, min_nr: c_long, nr: c_long, events: ?[*]aio.IoEvent, timeout: ?*linux.timespec) callconv(.c) c_int {
        const r = real("io_getevents");
        if (lifecycle.enter()) {
            defer lifecycle.leave();
            if (ready()) |st| switch (aio.getevents(st, ctx, min_nr, nr, events, timeout, aioFns())) {
                .done => |v| return v,
                .real => {},
            };
        }
        if (r) |f| return f(ctx, min_nr, nr, events, timeout);
        return sys.aioResult(linux.syscall5(.io_getevents, ctx, @bitCast(min_nr), @bitCast(nr), @intFromPtr(events), @intFromPtr(timeout)));
    }

    pub fn io_pgetevents(ctx: aio.io_context_t, min_nr: c_long, nr: c_long, events: ?[*]aio.IoEvent, timeout: ?*linux.timespec, sigmask: ?*const anyopaque) callconv(.c) c_int {
        const r = real("io_pgetevents");
        if (lifecycle.enter()) {
            defer lifecycle.leave();
            if (ready()) |st| switch (aio.getevents(st, ctx, min_nr, nr, events, timeout, aioFns())) {
                .done => |v| return v,
                .real => {},
            };
        }
        if (r) |f| return f(ctx, min_nr, nr, events, timeout, sigmask);
        return sys.aioResult(linux.syscall5(.io_getevents, ctx, @bitCast(min_nr), @bitCast(nr), @intFromPtr(events), @intFromPtr(timeout)));
    }

    pub fn io_cancel(ctx: aio.io_context_t, iocb: ?*const aio.Iocb, result: ?*aio.IoEvent) callconv(.c) c_int {
        const r = real("io_cancel");
        if (lifecycle.enter()) {
            defer lifecycle.leave();
            switch (aio.cancel(ctx, iocb)) {
                .done => |v| return v,
                .real => {},
            }
        }
        if (r) |f| return f(ctx, iocb, result);
        return sys.aioResult(linux.syscall3(.io_cancel, ctx, @intFromPtr(iocb), @intFromPtr(result)));
    }

    // ---- libaio's queue face: libaio's own bodies (io_queue_init.c,
    //      io_queue_release.c, io_queue_run.c, io_queue_wait.c) over the
    //      shim's wrappers above — NEVER the real symbols. libaio binds its
    //      internal calls locally (Ubuntu links it `-Bsymbolic-functions`:
    //      libaio.so.1 carries no `io_*` relocation), so the real
    //      `io_queue_init` reaches libaio's own `io_setup` and creates a
    //      context the registry never learns; every later `io_submit` on it
    //      was an uncounted passthrough (fio's libaio engine, F3). ----

    pub fn io_queue_init(maxevents: c_int, ctxp: ?*aio.io_context_t) callconv(.c) c_int {
        const p = ctxp orelse return -@as(c_int, @backingInt(linux.E.INVAL));
        if (maxevents <= 0) return -@as(c_int, @backingInt(linux.E.INVAL));
        // The kernel refuses a non-zero *ctxp; libaio clears it first.
        p.* = 0;
        return io_setup(maxevents, p);
    }

    pub fn io_queue_release(ctx: aio.io_context_t) callconv(.c) c_int {
        return io_destroy(ctx);
    }

    pub fn io_queue_run(ctx: aio.io_context_t) callconv(.c) c_int {
        return aio.queueRun(ctx, io_getevents);
    }

    pub fn io_queue_wait(ctx: aio.io_context_t, timeout: ?*linux.timespec) callconv(.c) c_int {
        return io_getevents(ctx, 0, 0, null, timeout);
    }

    // ---- the linked-mode API ----

    /// Classify and bind an fd the app already holds (linked mode: an fd
    /// opened before the shim was in play, or inherited). 1 = bound, 0 =
    /// kernel-served (its reason on the engagement ledger). The mirror is
    /// never armed: no fork-epoch snapshot preceded the fd's open.
    pub fn squeezefs_il_bind(fd: c_int) callconv(.c) c_int {
        if (!lifecycle.enter()) return 0;
        defer lifecycle.leave();
        const st = lifecycle.state() orelse return 0;
        return switch (classify.classifyAndBind(st, classify.Probes.real, fd, std.math.maxInt(u64))) {
            .bound => 1,
            .passthrough => 0,
        };
    }
};

// The exports: only in the `.so`.
comptime {
    if (lifecycle.in_so) {
        for (table) |e| @export(&@field(wrappers, e.name), .{ .name = e.name });
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the table: 65 interpositions + the API, unique names, every row has a wrapper named exactly as the symbol; libaio rows carry their default versions, the queue face included" {
    @setEvalBranchQuota(20_000);
    var n_api: usize = 0;
    inline for (table, 0..) |e, i| {
        if (e.api) n_api += 1;
        inline for (table[0..i]) |o| try testing.expect(!std.mem.eql(u8, o.name, e.name));
        try testing.expect(@hasDecl(wrappers, e.name));
        if (std.mem.startsWith(u8, e.name, "io_")) try testing.expect(e.version != null) else try testing.expect(e.version == null);
    }
    try testing.expectEqual(@as(usize, 1), n_api);
    try testing.expectEqual(INTERPOSED + 1, table.len);
    try testing.expectEqualStrings("LIBAIO_0.1", table[index("io_submit")].version.?);
    try testing.expectEqualStrings("LIBAIO_0.5", table[index("io_pgetevents")].version.?);
    // libaio's queue face: the four symbols fio's engine and io_queue_run
    // callers reach, at their exported default versions.
    try testing.expectEqualStrings("LIBAIO_0.1", table[index("io_queue_init")].version.?);
    try testing.expectEqualStrings("LIBAIO_0.1", table[index("io_queue_release")].version.?);
    try testing.expectEqualStrings("LIBAIO_0.1", table[index("io_queue_run")].version.?);
    try testing.expectEqualStrings("LIBAIO_0.4", table[index("io_queue_wait")].version.?);
}

test "io_queue_init is libaio's body over the shim's io_setup: maxevents ≤ 0 and a NULL ctxp answer −EINVAL before any syscall; a context it creates is in the registry (the ring will see its iocbs) and io_queue_release vacates it" {
    var ctx: aio.io_context_t = 0xDEAD;
    try testing.expectEqual(-@as(c_int, @backingInt(linux.E.INVAL)), wrappers.io_queue_init(0, &ctx));
    try testing.expectEqual(-@as(c_int, @backingInt(linux.E.INVAL)), wrappers.io_queue_init(-1, &ctx));
    try testing.expectEqual(-@as(c_int, @backingInt(linux.E.INVAL)), wrappers.io_queue_init(8, null));
    try testing.expectEqual(@as(aio.io_context_t, 0xDEAD), ctx);
    try testing.expectEqual(@as(c_int, 0), wrappers.io_queue_init(8, &ctx));
    try testing.expect(ctx != 0 and ctx != 0xDEAD);
    try testing.expect(aio.registry.lookup(ctx) != null);
    // A fresh io_queue_init clears *ctxp before the syscall (the kernel
    // refuses a non-zero value): a second context from the same variable.
    var ctx2 = ctx;
    try testing.expectEqual(@as(c_int, 0), wrappers.io_queue_init(8, &ctx2));
    try testing.expect(ctx2 != ctx);
    try testing.expectEqual(@as(c_int, 0), wrappers.io_queue_release(ctx2));
    try testing.expect(aio.registry.lookup(ctx2) == null);
    try testing.expectEqual(@as(c_int, 0), wrappers.io_queue_release(ctx));
    try testing.expect(aio.registry.lookup(ctx) == null);
    // io_queue_wait on a released context is the kernel's answer (−EINVAL).
    var ts: linux.timespec = .{ .sec = 0, .nsec = 0 };
    try testing.expectEqual(-@as(c_int, @backingInt(linux.E.INVAL)), wrappers.io_queue_wait(ctx, &ts));
}

test "linked mode in the test binary: the real symbols resolve through RTLD_NEXT (libc's), the wrappers pass a plain file's I/O through byte-exact, and nothing of the test binary is interposed" {
    try testing.expect(!lifecycle.in_so);
    try testing.expect(realResolved("pread64"));
    try testing.expect(realResolved("close"));
    try testing.expect(realResolved("open"));
    var pbuf: [64]u8 = undefined;
    const path = try std.fmt.bufPrintSentinel(&pbuf, "/tmp/sqz-il-interpose-{d}", .{linux.getpid()}, 0);
    defer _ = linux.unlinkat(linux.AT.FDCWD, path, 0);
    const flags: c_int = @bitCast(sys.O.RDWR | sys.O.CREAT | sys.O.CLOEXEC);
    const fd = wrappers.openFixed(path, flags, 0o600);
    try testing.expect(fd >= 0);
    const payload = "the plain-file passthrough battery";
    try testing.expectEqual(@as(isize, payload.len), wrappers.pwrite64(fd, @constCast(payload.ptr), payload.len, 0));
    var out: [64]u8 = undefined;
    try testing.expectEqual(@as(isize, payload.len), wrappers.pread64(fd, &out, out.len, 0));
    try testing.expectEqualStrings(payload, out[0..payload.len]);
    try testing.expectEqual(@as(i64, 0), wrappers.lseek(fd, 0, linux.SEEK.SET));
    try testing.expectEqual(@as(isize, 4), wrappers.read(fd, &out, 4));
    try testing.expectEqual(@as(i64, 4), wrappers.lseek(fd, 0, linux.SEEK.CUR));
    const d = wrappers.dup(fd);
    try testing.expect(d >= 0);
    try testing.expectEqual(@as(c_int, 0), wrappers.close(d));
    try testing.expectEqual(@as(c_int, 0), wrappers.close(fd));
    // A bad fd: the real call's errno.
    try testing.expectEqual(@as(isize, -1), wrappers.pread64(-1, &out, 1, 0));
    try testing.expectEqual(linux.E.BADF, sys.getErrno());
    // The plain file never engaged the ladder's control plane: no session.
    if (lifecycle.stateIfReady()) |st| try testing.expectEqual(@as(u32, 0), st.readySessions());
}
