//! Daemonization and the process-level plumbing of `squeezefs mount`
//! (charter §4 "an external mount supervisor (`--daemon --supervise` …)";
//! §8 "`mount` (`--daemon --supervise --log-file` 0600 `O_NOFOLLOW`)";
//! `docs/porting/fuse_abi_session.md` §9.1). Linux only.
//!
//! The double fork, with ONE pipe per hop so the invoking shell learns the
//! outcome of the bring-up and not merely that a process started:
//!
//!   P (the shell's child) ── pipe A ── C1 (setsid; the supervisor's seat)
//!                                        └── pipe B ── C2 (the daemon)
//!
//! C2 runs the mount and writes `ready pid=<pid>\n` to pipe B once the
//! session is armed, or the failure's text (then exits non-zero). C1 relays
//! pipe B to pipe A byte for byte and closes it, so P sees exactly what C2
//! said (EOF without a word = C2 died before reporting). P prints a failure
//! to stderr and exits 1; on `ready` it exits 0 silently. Without
//! `--supervise` C1 exits right after the relay (C2 is reparented to init);
//! with it C1 stays as the external supervisor (`supervisor.zig`) over C2,
//! which is its child — so it can reap it and name its wait status.
//!
//! The fork happens BEFORE the executor starts a single thread (the knob
//! gate and the argument parse are the only work done so far), so the
//! children never inherit a half-built thread state.
//!
//! Signals (charter §9 "stop latches, never abort from outside"): the
//! daemon installs ONE handler for SIGTERM/SIGINT (latch stop) and SIGUSR1
//! (dump the stats to the log — the `.stats` inode's stand-in until rung
//! 5); the handler only ORs a bit into an atomic and writes the daemon's
//! eventfd (both async-signal-safe); the main task waits on that eventfd
//! through its lane ring (`SqzIo.readable`) and acts in its own context.
//! SIGPIPE is ignored (a relay pipe closed early must not kill a mount).

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const log = @import("../core/log.zig");
const log_sink = @import("log_sink.zig");
const supervisor = @import("supervisor.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/daemon is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const ready_word = "ready";

// ---------------------------------------------------------------------------
// The log file.
// ---------------------------------------------------------------------------

pub const LogFileError = error{OpenFailed};

/// Open `path` as the daemon's log: `O_WRONLY|O_CREAT|O_APPEND|O_NOFOLLOW|
/// O_CLOEXEC`, mode 0600 (`log_sink.LOG_FILE_MODE`; a symlink at the path is
/// refused — a root daemon must never be steered into writing elsewhere).
/// Routes `std.log` there.
pub fn openLogFile(path: [:0]const u8, stderr: *Io.Writer) LogFileError!linux.fd_t {
    const rc = linux.open(path.ptr, .{ .ACCMODE = .WRONLY, .CREAT = true, .APPEND = true, .NOFOLLOW = true, .CLOEXEC = true }, log_sink.LOG_FILE_MODE);
    const e = linux.errno(rc);
    if (e != .SUCCESS) {
        stderr.print("squeezefs mount: cannot open --log-file {s} ({t}){s}\n", .{ path, e, if (e == .LOOP) " — it is a symlink, which O_NOFOLLOW refuses" else "" }) catch {};
        return error.OpenFailed;
    }
    const fd: linux.fd_t = @intCast(rc);
    // An existing file keeps its mode only if it is already private.
    _ = linux.fchmod(fd, log_sink.LOG_FILE_MODE);
    log_sink.install(fd);
    return fd;
}

// ---------------------------------------------------------------------------
// The ready pipe.
// ---------------------------------------------------------------------------

/// The daemon's end: one message, then closed.
pub const Reporter = struct {
    fd: linux.fd_t,
    sent: bool = false,

    /// `ready pid=<pid>\n`.
    pub fn ready(self: *Reporter) void {
        var buf: [64]u8 = undefined;
        const line = std.fmt.bufPrint(&buf, "{s} pid={d}\n", .{ ready_word, linux.getpid() }) catch unreachable;
        self.send(line);
    }

    /// The failure's text (one or more lines).
    pub fn fail(self: *Reporter, text: []const u8) void {
        self.send(text);
        if (text.len == 0 or text[text.len - 1] != '\n') self.send("\n");
    }

    fn send(self: *Reporter, bytes: []const u8) void {
        if (self.fd < 0) return;
        writeAll(self.fd, bytes);
    }

    pub fn close(self: *Reporter) void {
        if (self.fd < 0) return;
        _ = linux.close(self.fd);
        self.fd = -1;
    }
};

fn writeAll(fd: linux.fd_t, bytes: []const u8) void {
    var off: usize = 0;
    while (off < bytes.len) {
        const rc = linux.write(fd, bytes[off..].ptr, bytes.len - off);
        switch (linux.errno(rc)) {
            .SUCCESS => off += rc,
            .INTR => continue,
            else => return,
        }
    }
}

/// Read until EOF into `buf` (a short buffer truncates the tail).
fn readToEof(fd: linux.fd_t, buf: []u8) []const u8 {
    var n: usize = 0;
    while (n < buf.len) {
        const rc = linux.read(fd, buf[n..].ptr, buf.len - n);
        switch (linux.errno(rc)) {
            .SUCCESS => {
                if (rc == 0) break;
                n += rc;
            },
            .INTR => continue,
            else => break,
        }
    }
    return buf[0..n];
}

fn makePipe() ?[2]linux.fd_t {
    var fds: [2]linux.fd_t = undefined;
    if (linux.errno(linux.pipe2(&fds, .{ .CLOEXEC = true })) != .SUCCESS) return null;
    return fds;
}

// ---------------------------------------------------------------------------
// The double fork.
// ---------------------------------------------------------------------------

pub const Role = union(enum) {
    /// This process is the shell's child: the exit code to return.
    parent: u8,
    /// This process is the daemon: report through the reporter.
    daemon: Reporter,
};

pub const Options = struct {
    supervise: bool,
    /// The mountpoint, for the supervisor's probe.
    mountpoint: [:0]const u8,
    /// The open log file: C1 and C2 point stdout/stderr at it (panics and
    /// the runtime's own complaints land beside the log lines) and so
    /// release the invoking shell's streams at `ready`.
    log_fd: linux.fd_t,
    policy: supervisor.Policy = .{},
};

/// Fork twice (module doc). Returns `parent` in the invoking process once
/// the daemon reported, `daemon` in the daemon. C1 never returns: it exits
/// after the relay or after the supervisor loop.
pub fn detach(gpa: std.mem.Allocator, io: Io, opts: Options, stderr: *Io.Writer) Role {
    const pipe_a = makePipe() orelse {
        stderr.writeAll("squeezefs mount: pipe failed\n") catch {};
        return .{ .parent = 1 };
    };
    const pid1 = fork() orelse {
        stderr.writeAll("squeezefs mount: fork failed\n") catch {};
        return .{ .parent = 1 };
    };
    if (pid1 != 0) {
        // P: wait for the relayed report.
        _ = linux.close(pipe_a[1]);
        var buf: [4096]u8 = undefined;
        const report = readToEof(pipe_a[0], &buf);
        _ = linux.close(pipe_a[0]);
        // C1 is our child: reap it. Without `--supervise` it exits right
        // after the relay (a blocking wait of microseconds — never a
        // zombie); as the supervisor it outlives us by design and init
        // reaps it.
        var status: i32 = 0;
        _ = linux.waitpid(pid1, &status, if (opts.supervise) linux.W.NOHANG else 0);
        if (std.mem.startsWith(u8, report, ready_word)) return .{ .parent = 0 };
        if (report.len == 0) {
            stderr.writeAll("squeezefs mount: the daemon exited before reporting ready (see its log)\n") catch {};
        } else {
            stderr.print("squeezefs mount: {s}", .{report}) catch {};
            if (report[report.len - 1] != '\n') stderr.writeByte('\n') catch {};
        }
        stderr.flush() catch {};
        return .{ .parent = 1 };
    }
    // C1: a new session, the shell's streams released, then the daemon.
    _ = linux.close(pipe_a[0]);
    _ = linux.setsid();
    redirectStdinToNull();
    redirectStdioTo(opts.log_fd);
    const pipe_b = makePipe() orelse {
        writeAll(pipe_a[1], "pipe failed in the intermediate process\n");
        linux.exit(1);
    };
    const pid2 = fork() orelse {
        writeAll(pipe_a[1], "fork failed in the intermediate process\n");
        linux.exit(1);
    };
    if (pid2 == 0) {
        // C2: the daemon (stdio already C1's).
        _ = linux.close(pipe_a[1]);
        _ = linux.close(pipe_b[0]);
        return .{ .daemon = .{ .fd = pipe_b[1] } };
    }
    // C1: relay B → A, then exit or supervise.
    _ = linux.close(pipe_b[1]);
    var relay: [4096]u8 = undefined;
    const report = readToEof(pipe_b[0], &relay);
    _ = linux.close(pipe_b[0]);
    writeAll(pipe_a[1], report);
    _ = linux.close(pipe_a[1]);
    const ready = std.mem.startsWith(u8, report, ready_word);
    if (!ready) {
        // The daemon failed: reap it so no zombie survives us.
        var status: i32 = 0;
        _ = linux.waitpid(pid2, &status, 0);
        linux.exit(1);
    }
    if (!opts.supervise) linux.exit(0);
    linux.exit(supervisor.linux_loop.run(gpa, io, pid2, opts.mountpoint, opts.policy));
}

fn fork() ?linux.pid_t {
    const rc = linux.fork();
    if (linux.errno(rc) != .SUCCESS) return null;
    return @intCast(rc);
}

fn redirectStdinToNull() void {
    const rc = linux.open("/dev/null", .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return;
    const fd: linux.fd_t = @intCast(rc);
    _ = linux.dup2(fd, 0);
    _ = linux.close(fd);
}

fn redirectStdioTo(fd: linux.fd_t) void {
    _ = linux.dup2(fd, 1);
    _ = linux.dup2(fd, 2);
}

// ---------------------------------------------------------------------------
// Signals.
// ---------------------------------------------------------------------------

/// Pending-signal bits (ORed by the handler, swapped out by the main task).
pub const SIG_STOP: u32 = 1 << 0;
pub const SIG_STATS: u32 = 1 << 1;

/// The one process-wide signal word and the eventfd it rings. Installed
/// once per process (`Signals.install`), before any thread exists.
pub const Signals = struct {
    var pending: std.atomic.Value(u32) = .init(0);
    var efd: std.atomic.Value(i32) = .init(-1);

    /// The eventfd the main task waits on (`SqzIo.readable`).
    pub fn eventfd() linux.fd_t {
        return efd.load(.monotonic);
    }

    pub fn install() error{EventfdFailed}!void {
        try initEventfd();
        installHandlers();
    }

    fn initEventfd() error{EventfdFailed}!void {
        const rc = linux.eventfd(0, linux.EFD.CLOEXEC | linux.EFD.NONBLOCK);
        if (linux.errno(rc) != .SUCCESS) return error.EventfdFailed;
        efd.store(@intCast(rc), .monotonic);
    }

    fn installHandlers() void {
        var act: linux.Sigaction = .{ .handler = .{ .handler = handle }, .mask = linux.sigemptyset(), .flags = linux.SA.RESTART };
        _ = linux.sigaction(.TERM, &act, null);
        _ = linux.sigaction(.INT, &act, null);
        _ = linux.sigaction(.USR1, &act, null);
        var ign: linux.Sigaction = .{ .handler = .{ .handler = linux.SIG.IGN }, .mask = linux.sigemptyset(), .flags = 0 };
        _ = linux.sigaction(.PIPE, &ign, null);
    }

    /// Swap out the pending bits (the main task, after its wake).
    pub fn take() u32 {
        return pending.swap(0, .acq_rel);
    }

    /// Ring the eventfd from any context (the session's `on_shutdown` hook
    /// uses it too).
    pub fn ring() void {
        const fd = efd.load(.monotonic);
        if (fd < 0) return;
        const one: u64 = 1;
        _ = linux.write(fd, @ptrCast(&one), 8);
    }

    /// Drain the eventfd counter after a wake (level-triggered re-await).
    pub fn drain() void {
        const fd = efd.load(.monotonic);
        if (fd < 0) return;
        var v: u64 = 0;
        _ = linux.read(fd, @ptrCast(&v), 8);
    }

    fn handle(sig: linux.SIG) callconv(.c) void {
        const bit: u32 = switch (sig) {
            .TERM, .INT => SIG_STOP,
            .USR1 => SIG_STATS,
            else => return,
        };
        // release pairs with `take`'s acq_rel: the main task sees the bit
        // after the eventfd wake.
        _ = pending.fetchOr(bit, .release);
        ring();
    }
};

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the log file opens 0600 with O_APPEND and O_NOFOLLOW: a symlink at the path is refused, the sink is installed on success" {
    _ = linux.mkdir("/build/tmp", 0o755);
    var pb: [64:0]u8 = undefined;
    const path = try std.fmt.bufPrintSentinel(&pb, "/build/tmp/sqz-log-{d}.log", .{linux.getpid()}, 0);
    _ = linux.unlink(path.ptr);
    defer _ = linux.unlink(path.ptr);
    var err_buf: [256]u8 = undefined;
    var err_w: Io.Writer = .fixed(&err_buf);
    // A symlink at the path: refused with O_NOFOLLOW's ELOOP.
    _ = linux.symlink("/etc/hostname", path.ptr);
    try testing.expectError(error.OpenFailed, openLogFile(path, &err_w));
    try testing.expect(std.mem.indexOf(u8, err_w.buffered(), "symlink") != null);
    _ = linux.unlink(path.ptr);
    // A fresh file: mode 0600, append.
    const fd = try openLogFile(path, &err_w);
    defer _ = linux.close(fd);
    try testing.expect(log_sink.installed());
    var st: linux.Statx = undefined;
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.statx(linux.AT.FDCWD, path.ptr, 0, linux.STATX.BASIC_STATS, &st)));
    try testing.expectEqual(@as(u16, 0o600), st.mode & 0o777);
    try testing.expectEqual(@as(usize, @as(u32, @bitCast(linux.O{ .APPEND = true }))), linux.fcntl(fd, linux.F.GETFL, 0) & @as(usize, @as(u32, @bitCast(linux.O{ .APPEND = true }))));
    // Restore the sink for the rest of the test binary.
    log_sink.install(-1);
}

test "the reporter writes `ready pid=` or the failure text terminated by a newline" {
    const fds = makePipe().?;
    defer _ = linux.close(fds[0]);
    var r: Reporter = .{ .fd = fds[1] };
    r.ready();
    r.close();
    var buf: [128]u8 = undefined;
    const got = readToEof(fds[0], &buf);
    try testing.expect(std.mem.startsWith(u8, got, "ready pid="));
    try testing.expect(std.mem.endsWith(u8, got, "\n"));
    const fds2 = makePipe().?;
    defer _ = linux.close(fds2[0]);
    var f: Reporter = .{ .fd = fds2[1] };
    f.fail("volume busy");
    f.close();
    try testing.expectEqualStrings("volume busy\n", readToEof(fds2[0], &buf));
}

test "signals: the handler latches TERM as stop and USR1 as the stats bit, rings the eventfd once per signal; take swaps the bits out" {
    // The handler is driven directly: installing a SIGTERM disposition in
    // the test runner's process would swallow the build runner's own kill.
    try Signals.initEventfd();
    try testing.expectEqual(@as(u32, 0), Signals.take());
    Signals.handle(.USR1);
    var pfd = [_]linux.pollfd{.{ .fd = Signals.eventfd(), .events = linux.POLL.IN, .revents = 0 }};
    try testing.expect(linux.poll(&pfd, 1, 0) == 1);
    Signals.drain();
    try testing.expectEqual(SIG_STATS, Signals.take());
    try testing.expect(linux.poll(&pfd, 1, 0) == 0);
    Signals.handle(.TERM);
    Signals.handle(.INT);
    try testing.expect(linux.poll(&pfd, 1, 0) == 1);
    Signals.drain();
    try testing.expectEqual(SIG_STOP, Signals.take());
    try testing.expectEqual(@as(u32, 0), Signals.take());
    Signals.handle(.HUP);
    try testing.expectEqual(@as(u32, 0), Signals.take());
}
