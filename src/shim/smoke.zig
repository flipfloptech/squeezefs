//! THE SHIM SMOKE (venue, root, Linux): `libsqueezefs_il.so` beside
//! `./squeezefs` in the cwd (build.zig stages both), driven as a foreign
//! process would see it — `LD_PRELOAD` on `dd`/`sha256sum`/`cat`/`cp`/
//! `truncate`/`tar`, a C program over `pread`/`pwrite`/`lseek`/`fstat`/
//! `dup`/`mmap`/`fork`, and a libaio harness (local ABI mirrors, `dlopen`
//! of libaio through the global scope so a preloaded shim's interposers
//! win; the shim's own raw-syscall arm serves it where libaio is absent).
//!
//! Rows:
//!   1. the plain-file passthrough battery on ext4: byte parity + exit
//!      codes, the shim silent (zero session attempts);
//!   2. a SqueezeFS mount WITHOUT `-o interception` under LD_PRELOAD: pure
//!      passthrough (ENODATA ⇒ the negative cache), the daemon's
//!      `ipc_sessions_total` 0;
//!   3. the C program and the aio harness on the plain mount (passthrough
//!      parity, every bail-out path exercised without a session);
//!   4. with `-o interception` when the daemon accepts it: the engagement
//!      law (`ipc_sessions_total ≥ 1`, `ipc_ops_*` deltas account for the
//!      dd/sha256sum ops), the C program's bail-outs, the aio harness
//!      served, the pairing refusal of a `-dirty` build (counted, one
//!      line), the kill of the daemon mid-op ⇒ the app's call returns the
//!      passthrough result. A mount that refuses the option marks these
//!      rows PENDING on the host wave, loudly, and the test passes on
//!      rows 1–3.
//!
//! Venue IOPS are scoping only (the 1,017,548 warm / 622,112 device-true
//! rows are the reference-hardware bar).

const std = @import("std");
const testkit = @import("../core/testkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;

const clock = @import("../core/clock.zig");
const log = @import("../core/log.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("shim/smoke is Linux only");
}

const testing = std.testing;

const SO_NAME = "libsqueezefs_il.so";
const volume_len: u64 = 256 << 20;
const data_len: u64 = 2 << 30;
const ready_wait_ns: u64 = 60 * std.time.ns_per_s;
const exit_wait_ns: u64 = 60 * std.time.ns_per_s;

/// The last 4 KiB of a stream, for a failure report.
fn lastBytes(text: []const u8) []const u8 {
    return text[text.len -| 4096..];
}

fn sleepMs(ms: u64) void {
    const ts: linux.timespec = .{ .sec = @intCast(ms / 1000), .nsec = @intCast((ms % 1000) * std.time.ns_per_ms) };
    _ = linux.nanosleep(&ts, null);
}

fn alive(pid: linux.pid_t) bool {
    var buf: [64]u8 = undefined;
    const p = std.fmt.bufPrintSentinel(&buf, "/proc/{d}/stat", .{pid}, 0) catch return false;
    const rc = linux.open(p, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return false;
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    var text: [512]u8 = undefined;
    const n = linux.read(fd, &text, text.len);
    if (linux.errno(n) != .SUCCESS or n == 0) return false;
    // "pid (comm) STATE ..." — a zombie is not alive.
    const close_paren = std.mem.lastIndexOfScalar(u8, text[0..n], ')') orelse return false;
    return close_paren + 2 < n and text[close_paren + 2] != 'Z';
}

fn venueMissing(io: Io) ?testkit.Why {
    return testkit.venue.mount(io, &.{ "squeezefs", SO_NAME });
}

const Rig = struct {
    gpa: std.mem.Allocator,
    io: Io,
    bin: [:0]u8,
    so: [:0]u8,
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
    /// The environment of LD_PRELOAD'd processes.
    preload_env: std.process.Environ.Map,
    zig_exe: ?[]u8 = null,

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
            .so = try std.fmt.allocPrintSentinel(gpa, "{s}/{s}", .{ cwd_buf[0..n], SO_NAME }, 0),
            .work = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/shim-{d}", .{pid}, 0),
            .img = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/shim-{d}/vol.img", .{pid}, 0),
            .d0 = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/shim-{d}/d0.img", .{pid}, 0),
            .d1 = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/shim-{d}/d1.img", .{pid}, 0),
            .meta_uri = try std.fmt.allocPrint(gpa, "sqmeta:///build/tmp/shim-{d}/vol.img", .{pid}),
            .data_uri = try std.fmt.allocPrint(gpa, "sqdata:///build/tmp/shim-{d}/d0.img,/build/tmp/shim-{d}/d1.img", .{ pid, pid }),
            .mp = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/shim-mnt-{d}", .{pid}, 0),
            .log_path = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/shim-{d}/daemon.log", .{pid}, 0),
            .env = std.process.Environ.Map.init(gpa),
            .preload_env = std.process.Environ.Map.init(gpa),
        };
        const path = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/opt/zig";
        try self.env.put("PATH", path);
        // A dev tree's identity is `-dirty` on both ends: the seam admits
        // it (the pre-check and the daemon's screen both read it).
        try self.env.put("SQUEEZEFS_TEST_IPC_ALLOW_DIRTY", "1");
        try self.preload_env.put("PATH", path);
        try self.preload_env.put("LD_PRELOAD", self.so);
        try self.preload_env.put("SQUEEZEFS_TEST_IPC_ALLOW_DIRTY", "1");
        _ = linux.mkdir(self.work.ptr, 0o755);
        _ = linux.mkdir(self.mp.ptr, 0o755);
        // `zig cc` needs its caches: the venue's global cache when the test
        // inherited it, a scratch one under the work dir otherwise.
        var proc_env = try std.testing.environ.createMap(gpa);
        defer proc_env.deinit();
        const cache = try std.fmt.allocPrint(gpa, "{s}/zig-cache", .{self.work});
        defer gpa.free(cache);
        try self.env.put("ZIG_GLOBAL_CACHE_DIR", proc_env.get("ZIG_GLOBAL_CACHE_DIR") orelse cache);
        try self.env.put("ZIG_LOCAL_CACHE_DIR", cache);
        try self.env.put("HOME", "/root");
        // The C rows need a compiler: `zig cc` from PATH.
        if (self.sh(self.work, "command -v zig")) |r| {
            defer self.gpa.free(r.stderr);
            if (r.term.success() and std.mem.trim(u8, r.stdout, " \n").len != 0) {
                self.zig_exe = try gpa.dupe(u8, std.mem.trim(u8, r.stdout, " \n"));
            }
            self.gpa.free(r.stdout);
        } else |_| {}
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
        self.preload_env.deinit();
        if (self.zig_exe) |z| self.gpa.free(z);
        self.gpa.free(self.bin);
        self.gpa.free(self.so);
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

    fn squeezefs(self: *Rig, args: []const []const u8) !std.process.RunResult {
        var argv: [16][]const u8 = undefined;
        argv[0] = self.bin;
        for (args, 0..) |a, i| argv[i + 1] = a;
        return std.process.run(self.gpa, self.io, .{ .argv = argv[0 .. args.len + 1], .environ_map = &self.env });
    }

    fn freeRun(self: *Rig, r: std.process.RunResult) void {
        self.gpa.free(r.stdout);
        self.gpa.free(r.stderr);
    }

    fn sh(self: *Rig, cwd: []const u8, script: []const u8) !std.process.RunResult {
        return std.process.run(self.gpa, self.io, .{ .argv = &.{ "sh", "-e", "-c", script }, .cwd = .{ .path = cwd }, .environ_map = &self.env });
    }

    /// A shell script under LD_PRELOAD; both streams returned.
    fn preload(self: *Rig, cwd: []const u8, script: []const u8) !std.process.RunResult {
        return std.process.run(self.gpa, self.io, .{ .argv = &.{ "sh", "-e", "-c", script }, .cwd = .{ .path = cwd }, .environ_map = &self.preload_env });
    }

    fn preloadOk(self: *Rig, cwd: []const u8, script: []const u8) !std.process.RunResult {
        const mark = if (self.child != null) try self.logLen() else 0;
        const r = try self.preload(cwd, script);
        if (!r.term.success()) {
            // The gate log alone must name the cause: the streams' tails and
            // what the daemon (when one runs) logged during the script.
            const log_text: []const u8 = if (self.child != null) self.readLog() catch "" else "";
            defer self.gpa.free(log_text);
            std.debug.print("\n--- preload script failed ({f}) in {s} ---\n{s}\n--- stdout (tail) ---\n{s}\n--- stderr (tail) ---\n{s}\n--- daemon log during the script (tail) ---\n{s}\n", .{ r.term, cwd, script, lastBytes(r.stdout), lastBytes(r.stderr), lastBytes(log_text[@min(mark, log_text.len)..]) });
            self.freeRun(r);
            return error.ScriptFailed;
        }
        return r;
    }

    fn formatSet(self: *Rig) !void {
        for ([_][:0]u8{ self.img, self.d0, self.d1 }, 0..) |p, i| {
            const f = try Io.Dir.cwd().createFile(self.io, p, .{ .truncate = true });
            defer f.close(self.io);
            try f.setLength(self.io, if (i == 0) volume_len else data_len);
        }
        const r = try self.squeezefs(&.{ "format", self.meta_uri, self.data_uri });
        defer self.freeRun(r);
        if (!r.term.success()) std.debug.print("\nformat: {s}\n", .{r.stderr});
        try testing.expect(r.term.success());
    }

    /// Spawn the foreground mount; false = the daemon refused the options
    /// (its stderr is in the log).
    fn mount(self: *Rig, extra: []const []const u8) !bool {
        std.debug.assert(self.child == null);
        if (self.log_file == null) self.log_file = try Io.Dir.cwd().createFile(self.io, self.log_path, .{ .truncate = false });
        const from = try self.logLen();
        var argv: [16][]const u8 = undefined;
        var n: usize = 0;
        argv[n] = self.bin;
        n += 1;
        argv[n] = "mount";
        n += 1;
        for (extra) |a| {
            argv[n] = a;
            n += 1;
        }
        argv[n] = self.meta_uri;
        n += 1;
        argv[n] = self.mp;
        n += 1;
        self.child = try std.process.spawn(self.io, .{ .argv = argv[0..n], .environ_map = &self.env, .stdin = .ignore, .stdout = .{ .file = self.log_file.? }, .stderr = .{ .file = self.log_file.? } });
        self.child_pid = self.child.?.id.?;
        const deadline = clock.monotonicNs() + ready_wait_ns;
        while (true) {
            const text = try self.readLog();
            defer self.gpa.free(text);
            if (text.len > from and std.mem.indexOf(u8, text[from..], "ready: ") != null) return true;
            if (!alive(self.child_pid)) {
                var c = self.child.?;
                self.child = null;
                _ = c.wait(self.io) catch {};
                return false;
            }
            if (clock.monotonicNs() >= deadline) return error.Timeout;
            sleepMs(20);
        }
    }

    fn umount(self: *Rig) !void {
        const r = try self.squeezefs(&.{ "umount", self.mp });
        defer self.freeRun(r);
        if (!r.term.success()) std.debug.print("\numount: {s}\n", .{r.stderr});
        try testing.expect(r.term.success());
        var c = self.child.?;
        self.child = null;
        const deadline = clock.monotonicNs() + exit_wait_ns;
        while (alive(self.child_pid) and clock.monotonicNs() < deadline) sleepMs(20);
        if (alive(self.child_pid)) _ = linux.kill(self.child_pid, .KILL);
        _ = try c.wait(self.io);
    }

    fn kill9(self: *Rig) void {
        if (self.child) |*c| {
            _ = linux.kill(self.child_pid, .KILL);
            _ = c.wait(self.io) catch {};
            self.child = null;
        }
        _ = linux.umount2(self.mp.ptr, linux.MNT.DETACH);
    }

    fn readLog(self: *Rig) ![]u8 {
        return Io.Dir.cwd().readFileAlloc(self.io, self.log_path, self.gpa, .limited(16 << 20));
    }

    fn logLen(self: *Rig) !usize {
        const text = try self.readLog();
        defer self.gpa.free(text);
        return text.len;
    }

    /// SIGUSR1 → the stats families in the log; the tail from the mark.
    /// SIGUSR1's stats dump: the log past the mark (THIS dump alone — a
    /// `numberAfter` over the whole log would read the mount's first dump
    /// every time).
    fn statsTail(self: *Rig) ![]u8 {
        const mark = try self.logLen();
        _ = linux.kill(self.child_pid, .USR1);
        const deadline = clock.monotonicNs() + 10 * std.time.ns_per_s;
        while (clock.monotonicNs() < deadline) {
            const text = try self.readLog();
            defer self.gpa.free(text);
            if (text.len > mark and std.mem.indexOf(u8, text[mark..], "stats daemon: ") != null) return self.gpa.dupe(u8, text[mark..]);
            sleepMs(20);
        }
        return error.Timeout;
    }

    /// Compile a C source with `zig cc`; the binary path (owned) or null
    /// when no compiler is available.
    fn cc(self: *Rig, name: []const u8, source: []const u8) !?[]u8 {
        const zig = self.zig_exe orelse return null;
        const src_path = try std.fmt.allocPrint(self.gpa, "{s}/{s}.c", .{ self.work, name });
        defer self.gpa.free(src_path);
        try Io.Dir.cwd().writeFile(self.io, .{ .sub_path = src_path, .data = source });
        const bin_path = try std.fmt.allocPrint(self.gpa, "{s}/{s}", .{ self.work, name });
        errdefer self.gpa.free(bin_path);
        const r = try std.process.run(self.gpa, self.io, .{ .argv = &.{ zig, "cc", "-O1", "-o", bin_path, src_path }, .environ_map = &self.env });
        defer self.freeRun(r);
        if (!r.term.success()) {
            std.debug.print("\n--- zig cc {s} failed ---\n{s}\n", .{ name, r.stderr });
            return error.CompileFailed;
        }
        return bin_path;
    }
};

fn numberAfter(text: []const u8, key: []const u8) ?u64 {
    const i = std.mem.indexOf(u8, text, key) orelse return null;
    var j = i + key.len;
    while (j < text.len and (text[j] == ' ' or text[j] == ':')) j += 1;
    var k = j;
    while (k < text.len and text[k] >= '0' and text[k] <= '9') k += 1;
    if (k == j) return null;
    return std.fmt.parseInt(u64, text[j..k], 10) catch null;
}

/// The byte-parity battery: dd 64 MiB → sha256sum, cat, cp, truncate, tar,
/// all under LD_PRELOAD; prints the sha of the file and of a cp of it.
const battery_script =
    \\dd if=/dev/urandom of=f bs=1M count=64 status=none
    \\a=$(sha256sum f | cut -d' ' -f1)
    \\cp f g
    \\cat g > h
    \\b=$(sha256sum h | cut -d' ' -f1)
    \\test "$a" = "$b" || { echo "FAIL sha: f $a, h $b, g $(sha256sum g | cut -d' ' -f1); sizes $(stat -c %s f g h)" >&2; cmp f g >&2 || :; cmp f h >&2 || :; exit 1; }
    \\truncate -s 3M h
    \\s=$(stat -c %s h)
    \\test "$s" = 3145728 || { echo "FAIL truncate -s 3M: stat says $s" >&2; exit 1; }
    \\head -c 3M f | cmp - h
    \\tar cf t.tar f
    \\tar xf t.tar -O f | cmp - f
    \\dd if=f of=/dev/null bs=4k status=none
    \\echo "sha $a"
;

/// The C program: positional + offsetful I/O, lseek, fstat, dup, mmap
/// (the per-inode unbind), fork (the child's passthrough), exec (CLOEXEC
/// drops the session); byte parity asserted at every step; exit 0.
const c_program =
    \\#define _GNU_SOURCE
    \\#include <errno.h>
    \\#include <fcntl.h>
    \\#include <stdio.h>
    \\#include <stdlib.h>
    \\#include <string.h>
    \\#include <sys/mman.h>
    \\#include <sys/stat.h>
    \\#include <sys/uio.h>
    \\#include <sys/wait.h>
    \\#include <unistd.h>
    \\#define CHECK(c) do { if (!(c)) { fprintf(stderr, "FAIL %s:%d %s (errno %d)\n", __FILE__, __LINE__, #c, errno); return 2; } } while (0)
    \\int main(int argc, char **argv) {
    \\    if (argc < 2) return 3;
    \\    char buf[65536], out[65536];
    \\    for (int i = 0; i < 65536; i++) buf[i] = (char)(i * 31 + 7);
    \\    int fd = open(argv[1], O_RDWR | O_CREAT | O_TRUNC, 0644);
    \\    CHECK(fd >= 0);
    \\    CHECK(pwrite(fd, buf, 65536, 0) == 65536);
    \\    CHECK(pwrite(fd, buf, 4096, 1 << 20) == 4096);
    \\    CHECK(pread(fd, out, 65536, 0) == 65536 && memcmp(buf, out, 65536) == 0);
    \\    CHECK(lseek(fd, 0, SEEK_SET) == 0);
    \\    CHECK(write(fd, buf, 1000) == 1000);
    \\    CHECK(write(fd, buf + 1000, 1000) == 1000);
    \\    CHECK(lseek(fd, 0, SEEK_CUR) == 2000);
    \\    CHECK(lseek(fd, 500, SEEK_SET) == 500);
    \\    CHECK(read(fd, out, 1500) == 1500 && memcmp(buf + 500, out, 1500) == 0);
    \\    CHECK(lseek(fd, -1, SEEK_SET) == -1 && errno == EINVAL);
    \\    struct stat st; CHECK(fstat(fd, &st) == 0 && st.st_size == (1 << 20) + 4096);
    \\    CHECK(lseek(fd, 0, SEEK_END) == (1 << 20) + 4096);
    \\    struct iovec iov[2] = { { buf, 100 }, { buf + 100, 200 } };
    \\    CHECK(pwritev(fd, iov, 2, 8192) == 300);
    \\    CHECK(pread(fd, out, 300, 8192) == 300 && memcmp(buf, out, 300) == 0);
    \\    int d = dup(fd); CHECK(d >= 0);
    \\    CHECK(lseek(d, 0, SEEK_SET) == 0);
    \\    CHECK(read(fd, out, 10) == 10);
    \\    CHECK(lseek(d, 0, SEEK_CUR) == 10);
    \\    CHECK(close(fd) == 0);
    \\    CHECK(pread(d, out, 4096, 1 << 20) == 4096 && memcmp(buf, out, 4096) == 0);
    \\    char *m = mmap(NULL, 65536, PROT_READ, MAP_SHARED, d, 0);
    \\    CHECK(m != MAP_FAILED && memcmp(m, buf, 65536) == 0);
    \\    CHECK(pwrite(d, buf, 16, 0) == 16);
    \\    munmap(m, 65536);
    \\    pid_t pid = fork(); CHECK(pid >= 0);
    \\    if (pid == 0) {
    \\        char c[4096];
    \\        if (pread(d, c, 4096, 0) != 4096 || memcmp(c, buf, 4096) != 0) _exit(4);
    \\        if (pwrite(d, buf, 4096, 2 << 20) != 4096) _exit(5);
    \\        execl("/bin/true", "true", (char *)0);
    \\        _exit(6);
    \\    }
    \\    int status = 0; CHECK(waitpid(pid, &status, 0) == pid && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    \\    CHECK(pread(d, out, 4096, 2 << 20) == 4096 && memcmp(buf, out, 4096) == 0);
    \\    CHECK(fcntl(d, F_GETFL) >= 0);
    \\    struct flock fl = { .l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 1 };
    \\    CHECK(fcntl(d, F_SETLK, &fl) == 0);
    \\    CHECK(pwrite(d, buf, 8, 4096) == 8);
    \\    CHECK(close(d) == 0);
    \\    CHECK(close(d) == -1 && errno == EBADF);
    \\    puts("C_OK");
    \\    return 0;
    \\}
;

/// The libaio lifecycle harness: local ABI mirrors; the symbols resolved
/// through the GLOBAL scope (`dlopen(libaio, RTLD_GLOBAL)` when present —
/// Ubuntu 24.04's soname is `libaio.so.1t64` — else the shim's own
/// exports / the raw syscalls); `io_setup` BEFORE the file (the field
/// crash ordering) and after, then fio's shape (`io_queue_init` /
/// `io_queue_release`, which libaio binds to its OWN `io_setup` internally)
/// when the queue face resolves; 8 PWRITEs in one `io_submit`, a drain, 8
/// PREADs, byte parity, cookie identity; `events == submits`.
const aio_harness =
    \\#define _GNU_SOURCE
    \\#include <dlfcn.h>
    \\#include <errno.h>
    \\#include <fcntl.h>
    \\#include <stdint.h>
    \\#include <stdio.h>
    \\#include <stdlib.h>
    \\#include <string.h>
    \\#include <sys/syscall.h>
    \\#include <time.h>
    \\#include <unistd.h>
    \\typedef void *io_context_t;
    \\struct iocb { uint64_t data; uint32_t key; uint32_t aio_rw_flags; uint16_t aio_lio_opcode; uint16_t aio_reqprio; int32_t aio_fildes; uint64_t buf; uint64_t nbytes; int64_t offset; int64_t pad3; uint32_t flags; uint32_t resfd; };
    \\struct io_event { uint64_t data; uint64_t obj; int64_t res; int64_t res2; };
    \\typedef int (*setup_t)(int, io_context_t *);
    \\typedef int (*destroy_t)(io_context_t);
    \\typedef int (*submit_t)(io_context_t, long, struct iocb **);
    \\typedef int (*getevents_t)(io_context_t, long, long, struct io_event *, struct timespec *);
    \\static setup_t p_setup; static destroy_t p_destroy; static submit_t p_submit; static getevents_t p_getevents;
    \\static setup_t p_queue_init; static destroy_t p_queue_release;
    \\static int raw_setup(int n, io_context_t *c) { return (int)syscall(SYS_io_setup, n, c) < 0 ? -errno : 0; }
    \\static int raw_destroy(io_context_t c) { return syscall(SYS_io_destroy, c) < 0 ? -errno : 0; }
    \\static int raw_submit(io_context_t c, long n, struct iocb **ios) { long r = syscall(SYS_io_submit, c, n, ios); return r < 0 ? -errno : (int)r; }
    \\static int raw_getevents(io_context_t c, long mn, long n, struct io_event *e, struct timespec *t) { long r = syscall(SYS_io_getevents, c, mn, n, e, t); return r < 0 ? -errno : (int)r; }
    \\#define CHECK(c) do { if (!(c)) { fprintf(stderr, "FAIL %d %s (errno %d)\n", __LINE__, #c, errno); return 2; } } while (0)
    \\/* mode 0: open then io_setup; 1: io_setup first; 2: fio's shape, io_queue_init/io_queue_release */
    \\static int run(const char *path, int mode) {
    \\    io_context_t ctx = 0;
    \\    int fd = -1;
    \\    setup_t setup = mode == 2 ? p_queue_init : p_setup;
    \\    destroy_t destroy = mode == 2 ? p_queue_release : p_destroy;
    \\    if (mode == 1) CHECK(setup(32, &ctx) == 0);
    \\    fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644); CHECK(fd >= 0);
    \\    if (mode != 1) CHECK(setup(32, &ctx) == 0);
    \\    static char wbuf[8][4096], rbuf[8][4096];
    \\    struct iocb ios[8], *ptrs[8];
    \\    for (int i = 0; i < 8; i++) {
    \\        memset(wbuf[i], 'a' + i, 4096);
    \\        memset(&ios[i], 0, sizeof ios[i]);
    \\        ios[i].data = 100 + i; ios[i].aio_lio_opcode = 1; ios[i].aio_fildes = fd;
    \\        ios[i].buf = (uint64_t)(uintptr_t)wbuf[i]; ios[i].nbytes = 4096; ios[i].offset = (int64_t)i * 4096;
    \\        ptrs[i] = &ios[i];
    \\    }
    \\    CHECK(p_submit(ctx, 8, ptrs) == 8);
    \\    struct io_event ev[8]; int got = 0;
    \\    while (got < 8) { struct timespec ts = { 5, 0 }; int r = p_getevents(ctx, 1, 8 - got, ev + got, &ts); CHECK(r > 0); got += r; }
    \\    for (int i = 0; i < 8; i++) { int k = (int)(ev[i].data - 100); CHECK(k >= 0 && k < 8); CHECK(ev[i].obj == (uint64_t)(uintptr_t)&ios[k]); if (ev[i].res != 4096) { fprintf(stderr, "FAIL write res=%lld\n", (long long)ev[i].res); return 2; } }
    \\    for (int i = 0; i < 8; i++) { ios[i].aio_lio_opcode = 0; ios[i].buf = (uint64_t)(uintptr_t)rbuf[i]; ios[i].data = 200 + i; }
    \\    CHECK(p_submit(ctx, 8, ptrs) == 8);
    \\    got = 0;
    \\    while (got < 8) { struct timespec ts = { 5, 0 }; int r = p_getevents(ctx, 2, 8 - got, ev + got, &ts); CHECK(r > 0); got += r; }
    \\    for (int i = 0; i < 8; i++) CHECK(memcmp(wbuf[i], rbuf[i], 4096) == 0);
    \\    CHECK(destroy(ctx) == 0);
    \\    CHECK(close(fd) == 0);
    \\    return 0;
    \\}
    \\int main(int argc, char **argv) {
    \\    if (argc < 2) return 3;
    \\    void *h = dlopen("libaio.so.1t64", RTLD_NOW | RTLD_GLOBAL);
    \\    if (!h) h = dlopen("libaio.so.1", RTLD_NOW | RTLD_GLOBAL);
    \\    p_setup = (setup_t)dlsym(RTLD_DEFAULT, "io_setup"); p_destroy = (destroy_t)dlsym(RTLD_DEFAULT, "io_destroy");
    \\    p_submit = (submit_t)dlsym(RTLD_DEFAULT, "io_submit"); p_getevents = (getevents_t)dlsym(RTLD_DEFAULT, "io_getevents");
    \\    p_queue_init = (setup_t)dlsym(RTLD_DEFAULT, "io_queue_init"); p_queue_release = (destroy_t)dlsym(RTLD_DEFAULT, "io_queue_release");
    \\    const char *lane = "shim-or-libaio";
    \\    if (!p_setup || !p_destroy || !p_submit || !p_getevents) { p_setup = raw_setup; p_destroy = raw_destroy; p_submit = raw_submit; p_getevents = raw_getevents; lane = "raw-syscalls"; }
    \\    int r = run(argv[1], 1); if (r) return r;
    \\    r = run(argv[1], 0); if (r) return r;
    \\    int queue_face = p_queue_init && p_queue_release;
    \\    if (queue_face) { r = run(argv[1], 2); if (r) return r; }
    \\    printf("AIO_OK\n");
    \\    fprintf(stderr, "aio lane=%s libaio=%s queue_face=%s\n", lane, h ? "present" : "absent", queue_face ? "run" : "absent");
    \\    return 0;
    \\}
;

const CRows = struct {
    c_bin: ?[]u8 = null,
    aio_bin: ?[]u8 = null,

    fn build(rig: *Rig) !CRows {
        var self: CRows = .{};
        self.c_bin = try rig.cc("shim_c_program", c_program);
        self.aio_bin = try rig.cc("shim_aio_harness", aio_harness);
        return self;
    }

    fn deinit(self: *CRows, gpa: std.mem.Allocator) void {
        if (self.c_bin) |b| gpa.free(b);
        if (self.aio_bin) |b| gpa.free(b);
    }

    /// Run both programs against `dir/<name>` twice — WITHOUT the shim
    /// (the baseline) and under LD_PRELOAD — and hold the bail-out law: a
    /// passthrough run is indistinguishable from the baseline (same stdout,
    /// same exit status). On an armed mount (`served`) the preload run must
    /// succeed outright: the ring serves what kernel FUSE may refuse (its
    /// own aio write answers EIO on this daemon). Returns the preload
    /// runs' combined output (owned).
    fn run(self: *CRows, rig: *Rig, dir: []const u8, served: bool) ![]u8 {
        var out: std.Io.Writer.Allocating = .init(rig.gpa);
        errdefer out.deinit();
        const progs = [_]struct { bin: ?[]u8, file: []const u8, marker: []const u8 }{
            .{ .bin = self.c_bin, .file = "cprog.bin", .marker = "C_OK" },
            .{ .bin = self.aio_bin, .file = "aio.bin", .marker = "AIO_OK" },
        };
        for (progs) |p| {
            const b = p.bin orelse continue;
            const script = try std.fmt.allocPrint(rig.gpa, "{s} {s}/{s}", .{ b, dir, p.file });
            defer rig.gpa.free(script);
            const base = try rig.sh(dir, script);
            defer rig.freeRun(base);
            const pre = try rig.preload(dir, script);
            defer rig.freeRun(pre);
            if (served) {
                if (!pre.term.success() or std.mem.indexOf(u8, pre.stdout, p.marker) == null) {
                    std.debug.print("\n--- {s} under the shim on the armed mount failed ({f}) ---\n{s}\n{s}\n", .{ p.file, pre.term, pre.stdout, pre.stderr });
                    return error.ServedRowFailed;
                }
            } else {
                // The baseline's own verdict is named (a kernel-served aio
                // write that answers EIO on the plain mount is the daemon's
                // row, not the shim's — 0002 row 351).
                std.debug.print("\nshim smoke: {s} on {s} without the shim: {f}; {s}\n", .{ p.file, dir, base.term, if (std.mem.indexOf(u8, base.stderr, "FAIL")) |i| std.mem.trim(u8, base.stderr[i..@min(base.stderr.len, i + 80)], "\n") else "ok" });
                // The law: the shim's passthrough is the baseline, byte for byte.
                const same = std.meta.eql(base.term, pre.term) and std.mem.eql(u8, base.stdout, pre.stdout);
                if (!same) {
                    std.debug.print("\n--- {s}: the shim's passthrough differs from the baseline ---\nbaseline ({f}): {s}\n{s}\npreload ({f}): {s}\n{s}\n", .{ p.file, base.term, base.stdout, base.stderr, pre.term, pre.stdout, pre.stderr });
                    return error.PassthroughDiffers;
                }
                // The shim prints nothing on a passthrough.
                try testing.expect(std.mem.indexOf(u8, pre.stderr, "squeezefs-il") == null);
            }
            try out.writer.writeAll(pre.stdout);
            try out.writer.writeAll(pre.stderr);
        }
        return out.toOwnedSlice();
    }
};

test "shim smoke: the LD_PRELOAD battery — plain-file parity with the shim silent; a SqueezeFS mount without -o interception stays pure passthrough (ipc_sessions_total 0); the C program and the libaio harness; then the engagement rows on an interception mount (dd/sha through the ring, the C program's bail-outs, aio served, the -dirty pairing refusal counted, kill -9 mid-op ⇒ the passthrough result) or PENDING when the daemon refuses the option" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    var crows = try CRows.build(rig);
    defer crows.deinit(gpa);
    if (rig.zig_exe == null) std.debug.print("\nshim smoke: no `zig` in PATH — the C program and the aio harness rows are skipped\n", .{});

    // ---- row 1: the plain-file battery on ext4 ----
    {
        const r = try rig.preloadOk(rig.work, battery_script);
        defer rig.freeRun(r);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "sha ") != null);
        // The shim is silent on a foreign filesystem: no session attempt,
        // no line.
        try testing.expect(std.mem.indexOf(u8, r.stderr, "squeezefs-il") == null);
        const c = try crows.run(rig, rig.work, false);
        defer gpa.free(c);
        std.debug.print("\nshim smoke row 1 (ext4 passthrough): ok; {s}\n", .{if (std.mem.indexOf(u8, c, "aio lane=")) |i| std.mem.trim(u8, c[i..@min(c.len, i + 72)], "\n") else "aio n/a"});
    }

    // ---- row 2: a SqueezeFS mount WITHOUT interception ----
    try rig.formatSet();
    try testing.expect(try rig.mount(&.{}));
    {
        const r = try rig.preloadOk(rig.mp, battery_script);
        defer rig.freeRun(r);
        try testing.expect(std.mem.indexOf(u8, r.stderr, "squeezefs-il") == null);
        const c = try crows.run(rig, rig.mp, false);
        defer gpa.free(c);
        const tail = try rig.statsTail();
        defer gpa.free(tail);
        const sessions = numberAfter(tail, "\"ipc_sessions_total\":") orelse 0;
        try testing.expectEqual(@as(u64, 0), sessions);
        std.debug.print("shim smoke row 2 (plain mount passthrough): ok, ipc_sessions_total 0\n", .{});
    }
    try rig.umount();

    // ---- rows 4+: the interception mount ----
    const armed = try rig.mount(&.{ "-o", "interception" });
    if (!armed) {
        const text = try rig.readLog();
        defer gpa.free(text);
        std.debug.print("shim smoke rows 4+ PENDING: the daemon refused `-o interception` (the host wave lands it): {s}\n", .{if (std.mem.lastIndexOf(u8, text, "interception")) |i| std.mem.trim(u8, text[i -| 80..@min(text.len, i + 80)], "\n") else "(no diagnostic)"});
        return;
    }
    defer rig.kill9();
    // 4a: the pairing refusal of a mismatched identity — a preload process
    // WITHOUT the dirty seam refuses a `-dirty` daemon loud, once per
    // mount, and stays kernel-served (parity holds).
    {
        var strict = try rig.preload_env.clone(gpa);
        defer strict.deinit();
        _ = strict.swapRemove("SQUEEZEFS_TEST_IPC_ALLOW_DIRTY");
        const r = try std.process.run(gpa, io, .{ .argv = &.{ "sh", "-e", "-c", "dd if=/dev/urandom of=strict bs=1M count=4 status=none; sha256sum strict > strict.sum; sha256sum -c strict.sum" }, .cwd = .{ .path = rig.mp }, .environ_map = &strict });
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
        const refused = std.mem.indexOf(u8, r.stderr, "dev build identity") != null;
        std.debug.print("shim smoke row 4a (pairing): {s}\n", .{if (refused) "a -dirty identity refused once per process, kernel-served" else "clean identity (the refusal row needs a dirty tree)"});
        // The once-rule is per (mount, reason) PER PROCESS (`once.first`):
        // the script's dd and sha256sum each refuse once; every line the
        // shim printed is that one refusal.
        if (refused) try testing.expectEqual(std.mem.count(u8, r.stderr, "dev build identity"), std.mem.count(u8, r.stderr, "squeezefs-il:"));
    }
    // 4b: the engagement law — dd 64 MiB + sha256sum + cat/cp through the
    // shim; the daemon's ipc_* family accounts for the ops.
    {
        const before = try rig.statsTail();
        defer gpa.free(before);
        const sessions0 = numberAfter(before, "\"ipc_sessions_total\":") orelse 0;
        const r = try rig.preloadOk(rig.mp, battery_script);
        defer rig.freeRun(r);
        const c = try crows.run(rig, rig.mp, true);
        defer gpa.free(c);
        const after = try rig.statsTail();
        defer gpa.free(after);
        const sessions1 = numberAfter(after, "\"ipc_sessions_total\":") orelse 0;
        try testing.expect(sessions1 > sessions0);
        // Every ipc op counter the daemon exports (`ipc_ops_*`): the sum
        // must be at least the dd's 64 writes + sha256sum's reads.
        const ops_total = (numberAfter(after, "\"ipc_ops_read\":") orelse 0) + (numberAfter(after, "\"ipc_ops_write\":") orelse 0) - (numberAfter(before, "\"ipc_ops_read\":") orelse 0) - (numberAfter(before, "\"ipc_ops_write\":") orelse 0);
        std.debug.print("shim smoke row 4b (engagement): sessions +{d}, ipc_ops_* sum {d}; stderr: {s}\n", .{ sessions1 - sessions0, ops_total, std.mem.trim(u8, r.stderr, "\n") });
        try testing.expect(ops_total >= 64);
    }
    // 4c: kill -9 the daemon under a preloaded reader ⇒ the app's call
    // returns the passthrough result (the kernel's answer for a dead
    // FUSE mount is an error, never a hang or a crash of the app).
    {
        const script = try std.fmt.allocPrint(gpa, "(dd if=f of=/dev/null bs=1M status=none; echo \"dd rc=$?\") & sleep 0.2; kill -9 {d}; wait; echo KILL_ROW_DONE", .{rig.child_pid});
        defer gpa.free(script);
        const r = try rig.preload(rig.mp, script);
        defer rig.freeRun(r);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "KILL_ROW_DONE") != null);
        std.debug.print("shim smoke row 4c (kill -9 mid-op): {s}\n", .{std.mem.trim(u8, r.stdout, "\n")});
    }
}

/// The direct write arm's writer (decision 0003 §AY): `<file> <pages>
/// [fsync_every]` — pages of one repeated u64 sequence number pwritten
/// O_DIRECT at scattered offsets of the pre-sized file, `acked <off> <seq>`
/// per landed write and `synced <seq>` per fsync, line-flushed; the first
/// failed call is an `error` line and exit 1 (loud, never a hang).
const direct_writer_src =
    \\#define _GNU_SOURCE
    \\#include <errno.h>
    \\#include <fcntl.h>
    \\#include <stdio.h>
    \\#include <stdlib.h>
    \\#include <sys/stat.h>
    \\#include <unistd.h>
    \\int main(int argc, char **argv) {
    \\    if (argc < 3) return 3;
    \\    unsigned long pages = strtoul(argv[2], 0, 10);
    \\    unsigned long fs_every = argc > 3 ? strtoul(argv[3], 0, 10) : 0;
    \\    int fd = open(argv[1], O_RDWR | O_DIRECT);
    \\    if (fd < 0) { fprintf(stderr, "error open errno %d\n", errno); return 2; }
    \\    struct stat st; if (fstat(fd, &st)) return 2;
    \\    unsigned long npages = (unsigned long)st.st_size / 4096; if (!npages) return 3;
    \\    void *buf; if (posix_memalign(&buf, 4096, 4096)) return 2;
    \\    setvbuf(stdout, NULL, _IOLBF, 0);
    \\    for (unsigned long i = 1; i <= pages; i++) {
    \\        unsigned long off = ((i * 2654435761UL) % npages) * 4096;
    \\        unsigned long *w = buf; for (int k = 0; k < 512; k++) w[k] = i;
    \\        ssize_t n = pwrite(fd, buf, 4096, (off_t)off);
    \\        if (n != 4096) { fprintf(stderr, "error pwrite seq %lu off %lu: %zd errno %d\n", i, off, n, errno); return 1; }
    \\        printf("acked %lu %lu\n", off, i);
    \\        if (fs_every && i % fs_every == 0) {
    \\            if (fsync(fd)) { fprintf(stderr, "error fsync seq %lu errno %d\n", i, errno); return 1; }
    \\            printf("synced %lu\n", i);
    \\        }
    \\    }
    \\    return 0;
    \\}
;

/// `acked <off> <seq>` lines → the last acked sequence per offset; every
/// such page must read (through the kernel, no shim) as one repeated u64
/// ≥ that sequence (a write in flight at the kill may have landed newer).
fn verifyAcked(gpa: std.mem.Allocator, out: []const u8, path: [:0]const u8) !struct { pages: usize, acked: usize } {
    var last = std.AutoHashMapUnmanaged(u64, u64).empty;
    defer last.deinit(gpa);
    var acked: usize = 0;
    var it = std.mem.splitScalar(u8, out, '\n');
    while (it.next()) |line| {
        if (!std.mem.startsWith(u8, line, "acked ")) continue;
        var f = std.mem.tokenizeScalar(u8, line[6..], ' ');
        const off = std.fmt.parseInt(u64, f.next() orelse continue, 10) catch continue;
        const seq = std.fmt.parseInt(u64, f.next() orelse continue, 10) catch continue;
        try last.put(gpa, off, seq);
        acked += 1;
    }
    const rc = linux.open(path, .{ .ACCMODE = .RDONLY, .DIRECT = true, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return error.OpenFailed;
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    const page = try gpa.alignedAlloc(u8, .fromByteUnits(4096), 4096);
    defer gpa.free(page);
    var pages: usize = 0;
    var kv = last.iterator();
    while (kv.next()) |e| {
        if (linux.pread(fd, page.ptr, 4096, @intCast(e.key_ptr.*)) != 4096) return error.ReadFailed;
        const words = std.mem.bytesAsSlice(u64, page);
        const first = words[0];
        for (words) |w| if (w != first) {
            std.debug.print("\n--- page @{d}: torn ({d} vs {d}) ---\n", .{ e.key_ptr.*, first, w });
            return error.TornPage;
        };
        if (first < e.value_ptr.*) {
            std.debug.print("\n--- page @{d}: acked seq {d}, device holds {d} ---\n", .{ e.key_ptr.*, e.value_ptr.*, first });
            return error.AckedWriteLost;
        }
        pages += 1;
    }
    return .{ .pages = pages, .acked = acked };
}

test "shim smoke: THE DIRECT WRITE ARM — a `-o interception=direct` mount, a root writer under LD_PRELOAD pwriting pages straight to the device under write leases: kill -9 the WRITER mid-run ⇒ every acked page reads back (nothing acked is lost); kill -9 the DAEMON with writes in flight ⇒ the writer fails loud and, after a remount, every acked page reads back (the device holds only acked bytes, the layout unchanged); the dm-log-writes cut after fsync'd writes ⇒ every synced page survives the replay to the last flush" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    // The C rows need `zig cc` on the venue's PATH; without it the row says
    // so and passes (the in-process e2e holds the arm's law regardless).
    const writer = (try rig.cc("direct_writer", direct_writer_src)) orelse {
        std.debug.print("shim smoke rows 5a-5c (the direct write arm): PENDING — no `zig cc` on PATH for the writer\n", .{});
        return;
    };
    defer gpa.free(writer);
    try rig.formatSet();
    const armed = try rig.mount(&.{ "-o", "interception=direct" });
    try testing.expect(armed);
    defer rig.kill9();
    var fpath: [256]u8 = undefined;
    const f = try std.fmt.bufPrintSentinel(&fpath, "{s}/f", .{rig.mp}, 0);
    // The resident file: 8 MiB through the kernel, no shim.
    {
        const r = try rig.sh(rig.mp, "dd if=/dev/zero of=f bs=1M count=8 oflag=direct conv=fsync status=none");
        defer rig.freeRun(r);
        try testing.expect(r.term.success());
    }
    // 5a: the writer killed mid-run.
    {
        const script = try std.fmt.allocPrint(gpa, "{s} f 100000000 > {s}/writer-a.out 2> {s}/writer-a.err & w=$!; sleep 0.4; kill -9 $w; rc=0; wait $w || rc=$?; echo \"writer rc=$rc\"", .{ writer, rig.work, rig.work });
        defer gpa.free(script);
        const r = try rig.preload(rig.mp, script);
        defer rig.freeRun(r);
        var ob: [256]u8 = undefined;
        const out = try Io.Dir.cwd().readFileAlloc(io, try std.fmt.bufPrint(&ob, "{s}/writer-a.out", .{rig.work}), gpa, .limited(64 << 20));
        defer gpa.free(out);
        const v = try verifyAcked(gpa, out, f);
        try testing.expect(v.acked > 100);
        const tail = try rig.statsTail();
        defer gpa.free(tail);
        const direct_writes = numberAfter(tail, "\"ipc_direct_writes\":") orelse 0;
        try testing.expect(direct_writes > 0);
        try testing.expectEqual(@as(u64, 0), numberAfter(tail, "\"ipc_write_leases_forced\":") orelse 0);
        try testing.expectEqual(@as(u64, 0), numberAfter(tail, "\"ipc_write_leases_live\":") orelse 0);
        std.debug.print("shim smoke row 5a (kill -9 the writer): {d} pages acked, every last-acked page intact; ipc_direct_writes {d}, leases forced 0; {s}\n", .{ v.acked, direct_writes, std.mem.trim(u8, r.stdout, "\n") });
    }
    // 5b: the daemon killed with writes in flight.
    {
        // (The writer's streams land OUTSIDE the mount: it is dead after the kill.)
        const script = try std.fmt.allocPrint(gpa, "{s} {s}/f 100000000 > {s}/writer-b.out 2> {s}/writer-b.err & w=$!; sleep 0.4; kill -9 {d}; rc=0; wait $w || rc=$?; echo \"writer rc=$rc\"; cat {s}/writer-b.err", .{ writer, rig.mp, rig.work, rig.work, rig.child_pid, rig.work });
        defer gpa.free(script);
        const r = try rig.preload(rig.work, script);
        defer rig.freeRun(r);
        // Loud: a non-zero exit with an error line; never a hang (the
        // heartbeat ladder poisons within a second).
        try testing.expect(std.mem.indexOf(u8, r.stdout, "writer rc=0") == null);
        try testing.expect(std.mem.indexOf(u8, r.stdout, "error ") != null);
        var ob: [256]u8 = undefined;
        const out = try Io.Dir.cwd().readFileAlloc(io, try std.fmt.bufPrint(&ob, "{s}/writer-b.out", .{rig.work}), gpa, .limited(64 << 20));
        defer gpa.free(out);
        rig.kill9();
        const remounted = try rig.mount(&.{ "-o", "interception=direct" });
        try testing.expect(remounted);
        const v = try verifyAcked(gpa, out, f);
        try testing.expect(v.acked > 100);
        std.debug.print("shim smoke row 5b (kill -9 the daemon under writes): {d} pages acked before the kill, every last-acked page intact after the remount; the writer: {s}\n", .{ v.acked, std.mem.trim(u8, r.stdout, "\n") });
    }
    // 5c: the power cut (dm-log-writes) after fsync'd direct writes: the
    //     script (`direct_cut.sh`, embedded) over loop devices; SKIP when
    //     the venue has no dm-log-writes or no replay-log.
    {
        var cut_dir: [256]u8 = undefined;
        const w = try std.fmt.bufPrint(&cut_dir, "{s}/cut", .{rig.work});
        _ = linux.mkdir(@ptrCast(w.ptr), 0o755);
        var sp_buf: [256]u8 = undefined;
        const sp = try std.fmt.bufPrint(&sp_buf, "{s}/direct_cut.sh", .{rig.work});
        try Io.Dir.cwd().writeFile(io, .{ .sub_path = sp, .data = direct_cut_script });
        const r = try std.process.run(gpa, io, .{ .argv = &.{ "bash", sp, rig.bin, rig.so, writer, w }, .environ_map = &rig.env });
        defer rig.freeRun(r);
        std.debug.print("shim smoke row 5c (the dm-log-writes cut): {s}\n", .{std.mem.trim(u8, r.stdout, "\n")});
        if (std.mem.indexOf(u8, r.stdout, "direct_cut: SKIP") == null) {
            if (!r.term.success()) std.debug.print("--- direct_cut stderr ---\n{s}\n", .{r.stderr});
            try testing.expect(r.term.success());
            try testing.expect(std.mem.indexOf(u8, r.stdout, "direct_cut: PASS") != null);
        }
    }
}

/// The power-cut row's script (its header is the method).
const direct_cut_script = @embedFile("direct_cut.sh");

test "shim smoke: RING APPENDS ACROSS TWO MEMBERS LOSE NOTHING — A (the manager) and B JOINED, both `-o interception`: A writes `own/f` (1 MiB, synced) in its slot; under LD_PRELOAD, B appends through its ring (a `dd` at the file's end on a plain O_WRONLY fd — the shim refuses O_APPEND, so the end is the size B's `stat` saw), then six rounds alternate a ring append on A while B holds a fresh grant and a ring append on B — both mounts read `f` byte-exact against a mirror, at once and after both remount; both rings served writes (`ipc_ops_write`); fsck --offline --scrub 0 findings" {
    const io = testing.io;
    if (venueMissing(io)) |why| return testkit.skipWhy(@src(), why);
    const gpa = testing.allocator;
    const rig = try Rig.init(gpa, io);
    defer rig.deinit();
    try rig.env.put("SQUEEZEFS_MW_BIND", "127.0.0.1:0");
    try rig.formatSet();
    try testing.expect(try rig.mount(&.{ "-o", "interception" }));
    const mp_b = try std.fmt.allocPrintSentinel(gpa, "{s}-b", .{rig.mp}, 0);
    defer gpa.free(mp_b);
    _ = linux.mkdir(mp_b.ptr, 0o755);
    defer _ = linux.rmdir(mp_b.ptr);
    const log_b = try std.fmt.allocPrintSentinel(gpa, "{s}/daemon-b.log", .{rig.work}, 0);
    defer gpa.free(log_b);
    var b_child: ?std.process.Child = null;
    defer if (b_child) |*c| {
        _ = linux.kill(c.id.?, .KILL);
        _ = c.wait(io) catch {};
        _ = linux.umount2(mp_b.ptr, linux.MNT.DETACH);
    };
    const spawnB = struct {
        fn run(r: *Rig, mp: [:0]const u8, log_path: [:0]const u8) !std.process.Child {
            // A fresh log per spawn: the child writes at the description's
            // offset, so a reused file would be overwritten from its start.
            const f = try Io.Dir.cwd().createFile(r.io, log_path, .{ .truncate = true });
            defer f.close(r.io);
            const from: u64 = 0;
            const c = try std.process.spawn(r.io, .{ .argv = &.{ r.bin, "mount", "-o", "interception", r.meta_uri, mp }, .environ_map = &r.env, .stdin = .ignore, .stdout = .{ .file = f }, .stderr = .{ .file = f } });
            const deadline = clock.monotonicNs() + ready_wait_ns;
            while (clock.monotonicNs() < deadline) {
                const text = try Io.Dir.cwd().readFileAlloc(r.io, log_path, r.gpa, .limited(64 << 20));
                defer r.gpa.free(text);
                if (text.len > from and std.mem.indexOf(u8, text[@intCast(from)..], "ready: ") != null) return c;
                sleepMs(20);
            }
            return error.Timeout;
        }
    }.run;
    b_child = try spawnB(rig, mp_b, log_b);
    var sb: [2048]u8 = undefined;
    // A's file and its mirror; B's ring append at the end its stat saw.
    {
        const r = try rig.sh(rig.mp, try std.fmt.bufPrint(&sb, "m=\"{s}/m\"; mkdir -p \"$m\" own; head -c 1048576 /dev/urandom > \"$m/f\"; cp \"$m/f\" own/f; sync -d own/f", .{rig.work}));
        rig.freeRun(r);
    }
    const ring_append =
        \\ap() {{ n=$(stat -c %s "$1"); printf '%s\n' "$2" | dd of="$1" bs=64 seek=$n oflag=seek_bytes conv=notrunc status=none; printf '%s\n' "$2" >> "$3"; }}
    ;
    {
        const r = try rig.preloadOk(mp_b, try std.fmt.bufPrint(&sb, ring_append ++ "\nap own/f b-first {s}/m/f\ni=1; while [ $i -le 6 ]; do stat -c %s own/f > /dev/null; ap {s}/own/f a-line-$i {s}/m/f; ap own/f b-line-$i {s}/m/f; i=$((i + 1)); done", .{ rig.work, rig.mp, rig.work, rig.work }));
        rig.freeRun(r);
    }
    const judge = "cmp own/f \"$0/m/f\" && echo same";
    for ([_][:0]const u8{ mp_b, rig.mp }) |mp| {
        const r = try std.process.run(gpa, io, .{ .argv = &.{ "sh", "-c", judge, rig.work }, .cwd = .{ .path = mp }, .environ_map = &rig.env });
        defer rig.freeRun(r);
        if (!std.mem.eql(u8, r.stdout, "same\n")) {
            std.debug.print("\n--- ring appends through {s}: {s}{s}\n", .{ mp, r.stdout, r.stderr });
            return error.RingAppendBytesLost;
        }
    }
    for ([_][:0]const u8{ mp_b, rig.mp }) |mp| {
        const r = try std.process.run(gpa, io, .{ .argv = &.{ "cat", ".stats" }, .cwd = .{ .path = mp }, .environ_map = &rig.env });
        defer rig.freeRun(r);
        try testing.expect((numberAfter(r.stdout, "\"ipc_ops_write\":") orelse 0) >= 6);
    }
    // Both remount: B first, then A; fsck; then both again and the judge.
    {
        const r = try rig.squeezefs(&.{ "umount", mp_b });
        rig.freeRun(r);
        var c = b_child.?;
        b_child = null;
        _ = try c.wait(io);
    }
    try rig.umount();
    {
        const r = try rig.squeezefs(&.{ "fsck", "--offline", "--scrub", rig.meta_uri });
        defer rig.freeRun(r);
        if (std.mem.indexOf(u8, r.stdout, "findings: 0 (clean)") == null) std.debug.print("\n--- fsck: {s}{s}\n", .{ r.stdout, r.stderr });
        try testing.expect(std.mem.indexOf(u8, r.stdout, "findings: 0 (clean)") != null);
    }
    try testing.expect(try rig.mount(&.{ "-o", "interception" }));
    b_child = try spawnB(rig, mp_b, log_b);
    for ([_][:0]const u8{ mp_b, rig.mp }) |mp| {
        const r = try std.process.run(gpa, io, .{ .argv = &.{ "sh", "-c", judge, rig.work }, .cwd = .{ .path = mp }, .environ_map = &rig.env });
        defer rig.freeRun(r);
        try testing.expectEqualStrings("same\n", r.stdout);
    }
    {
        const r = try rig.squeezefs(&.{ "umount", mp_b });
        rig.freeRun(r);
        var c = b_child.?;
        b_child = null;
        _ = try c.wait(io);
    }
    try rig.umount();
    std.debug.print("shim smoke: ring appends across two members: B's and A's interposed appends byte-exact through both mounts at once and after both remount; fsck clean\n", .{});
}
