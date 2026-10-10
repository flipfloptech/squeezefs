//! The external mount supervisor (charter §4 "Also ship: an external mount
//! supervisor (`--daemon --supervise`: stat a control inode every 5 s;
//! after 30 s dump task stacks, SIGABRT→SIGKILL, then write
//! `/sys/fs/fuse/connections/<minor>/abort` if `waiting > 0`)";
//! `docs/porting/fuse_abi_session.md` §9.1, Q10).
//!
//! It runs in the daemon's PARENT process (the intermediate of the double
//! fork, `daemon.zig`), std-only, no executor: it must outlive a wedged
//! daemon whose every thread is parked. The pure state machine `State` is
//! portable and tested; `run` is the Linux loop over it.
//!
//! Constants with their reasons:
//! - `PROBE_INTERVAL_NS` 5 s: the probe is a `stat` of `<mountpoint>/.stats`
//!   (the charter's control inode: a LOOKUP-only entry with TTL 0, so every
//!   probe REACHES the daemon as a LOOKUP and renders a payload — a root
//!   `stat` under the 1 s attr TTL would measure the kernel's dentry cache,
//!   not the daemon; `stats_cli.md` §7 Q19); 5 s is one daemon watchdog
//!   tick, the slowest periodic cadence the daemon already runs.
//! - `UNRESPONSIVE_NS` 30 s: six missed probes = six ticks of the daemon's
//!   own 5 s watchdog, long enough that a cold-device checkpoint or a
//!   ring-admission park never trips it (both are bounded in seconds, and
//!   the watchdog logs them), short enough that a wedged mount is freed
//!   before an application's own I/O timeouts (the JuiceFS precedent the
//!   reference cites).
//! - `ABORT_GRACE_NS` 2 s between SIGABRT and SIGKILL: the lock plane's tick
//!   (`sync/tick.zig`) — the time the daemon's own abort handler needs to
//!   write a core with the wedge in it; a daemon that cannot even die in
//!   one tick is killed.
//! - The sysfs `abort` write is the LAST rung and conditional on
//!   `waiting > 0`: it is destructive (ECONNABORTED to every blocked
//!   caller) and only needed when the kernel still holds requests the dead
//!   daemon can never answer.

const std = @import("std");
const fuzzkit = @import("../core/fuzzkit.zig");
const builtin = @import("builtin");
const tick_law = @import("../sync/tick.zig");

pub const PROBE_INTERVAL_NS: u64 = 5 * std.time.ns_per_s;
pub const UNRESPONSIVE_NS: u64 = 30 * std.time.ns_per_s;
pub const ABORT_GRACE_NS: u64 = tick_law.tick_ns;

pub const Policy = struct {
    probe_interval_ns: u64 = PROBE_INTERVAL_NS,
    unresponsive_after_ns: u64 = UNRESPONSIVE_NS,
    write_abort: bool = true,
};

pub const Verdict = union(enum) {
    healthy,
    /// Probes had been failing and one succeeded: re-armed.
    recovered,
    /// Failing, inside the window: `for_ns` so far.
    grace: u64,
    /// Past the window: escalate exactly once until a success re-arms.
    escalate: u64,
    /// Already escalated and still failing: nothing more to do.
    escalated,
};

/// The pure machine (`SupervisorState::observe`).
pub const State = struct {
    last_ok_ns: u64,
    failing: bool = false,
    escalated: bool = false,

    pub fn init(now_ns: u64) State {
        return .{ .last_ok_ns = now_ns };
    }

    pub fn observe(self: *State, now_ns: u64, ok: bool, policy: Policy) Verdict {
        if (ok) {
            const was_failing = self.failing;
            self.last_ok_ns = now_ns;
            self.failing = false;
            self.escalated = false;
            return if (was_failing) .recovered else .healthy;
        }
        self.failing = true;
        const for_ns = now_ns -| self.last_ok_ns;
        if (for_ns < policy.unresponsive_after_ns) return .{ .grace = for_ns };
        if (self.escalated) return .escalated;
        self.escalated = true;
        return .{ .escalate = for_ns };
    }
};

/// `waiting` is a decimal count on one line.
pub fn parseWaiting(text: []const u8) ?u64 {
    const t = std.mem.trim(u8, text, " \t\r\n");
    if (t.len == 0) return null;
    return std.fmt.parseInt(u64, t, 10) catch null;
}

pub const linux_loop = if (builtin.os.tag == .linux) struct {
    const linux = std.os.linux;
    const Io = std.Io;
    const log = @import("../core/log.zig");
    const clock = @import("../core/clock.zig");
    const thread_name = @import("../core/thread_name.zig");
    const completion = @import("../sync/completion.zig");

    pub const sysfs_connections = "/sys/fs/fuse/connections";

    /// A probe runs on its own thread so a wedged mount cannot wedge the
    /// supervisor: the cell is shared, the LAST party to touch it frees it
    /// (a timed-out probe thread is otherwise leaked by design — it is
    /// parked in the kernel on the mount).
    const Probe = struct {
        gpa: std.mem.Allocator,
        path: [:0]u8,
        done: completion.Completion(bool) = .{},
        /// Two parties; the one that drops it to 0 frees.
        refs: std.atomic.Value(u32) = .init(2),

        fn release(self: *Probe) void {
            // acq_rel: the freeing side sees the other side's last write.
            if (self.refs.fetchSub(1, .acq_rel) == 1) {
                self.gpa.free(self.path);
                self.gpa.destroy(self);
            }
        }

        fn main(self: *Probe) void {
            var st: linux.Statx = undefined;
            const rc = linux.statx(linux.AT.FDCWD, self.path.ptr, 0, linux.STATX.BASIC_STATS, &st);
            _ = self.done.complete(linux.errno(rc) == .SUCCESS);
            self.release();
        }
    };

    /// `stat(<mountpoint>/.stats)` within `timeout_ns`; false on a timeout
    /// or an error.
    pub fn probe(gpa: std.mem.Allocator, mountpoint: [:0]const u8, timeout_ns: u64) bool {
        const path = std.fmt.allocPrintSentinel(gpa, "{s}/.stats", .{mountpoint}, 0) catch return false;
        const p = gpa.create(Probe) catch {
            gpa.free(path);
            return false;
        };
        p.* = .{ .gpa = gpa, .path = path };
        const t = std.Thread.spawn(.{}, Probe.main, .{p}) catch {
            gpa.destroy(p);
            return false;
        };
        t.detach();
        const verdict = p.done.waitTimeout(timeout_ns) orelse false;
        p.release();
        return verdict;
    }

    /// The mount's connection id at START (a wedged mount wedges stat too):
    /// `/sys/fs/fuse/connections/<id>` is keyed by the mount's `st_dev`
    /// MINOR, which `statx` reports directly.
    pub fn connectionId(path: [:0]const u8) ?u64 {
        var st: linux.Statx = undefined;
        const rc = linux.statx(linux.AT.FDCWD, path.ptr, 0, linux.STATX.BASIC_STATS, &st);
        if (linux.errno(rc) != .SUCCESS) return null;
        return st.dev_minor;
    }

    fn sleepNs(ns: u64) void {
        const ts: linux.timespec = .{ .sec = @intCast(ns / std.time.ns_per_s), .nsec = @intCast(ns % std.time.ns_per_s) };
        _ = linux.nanosleep(&ts, null);
    }

    fn alive(pid: linux.pid_t) bool {
        return linux.errno(linux.kill(pid, @fromBackingInt(@intCast(0)))) == .SUCCESS;
    }

    /// Reap the daemon when it exited (it is our child); true once reaped.
    fn reap(pid: linux.pid_t, status: *i32) bool {
        const rc = linux.waitpid(pid, status, linux.W.NOHANG);
        return linux.errno(rc) == .SUCCESS and @as(linux.pid_t, @intCast(rc)) == pid;
    }

    /// `/proc/<pid>/status` + every task's `comm`, `wchan`, `stack` (root
    /// only; unreadable files are skipped silently) into the log.
    pub fn dumpDaemonState(io: Io, pid: linux.pid_t) void {
        var pb: [64]u8 = undefined;
        var buf: [16384]u8 = undefined;
        const status_path = std.fmt.bufPrint(&pb, "/proc/{d}/status", .{pid}) catch return;
        if (Io.Dir.cwd().readFile(io, status_path, &buf)) |text| {
            log.err("supervisor: {s}:\n{s}", .{ status_path, text });
        } else |_| {}
        const task_dir = std.fmt.bufPrint(&pb, "/proc/{d}/task", .{pid}) catch return;
        var dir = Io.Dir.cwd().openDir(io, task_dir, .{ .iterate = true }) catch return;
        defer dir.close(io);
        var it = dir.iterate();
        while (it.next(io) catch null) |ent| {
            var comm_buf: [64]u8 = undefined;
            var wchan_buf: [128]u8 = undefined;
            var stack_buf: [4096]u8 = undefined;
            var fp: [128]u8 = undefined;
            const comm = readTaskFile(io, &fp, task_dir, ent.name, "comm", &comm_buf);
            const wchan = readTaskFile(io, &fp, task_dir, ent.name, "wchan", &wchan_buf);
            const stack = readTaskFile(io, &fp, task_dir, ent.name, "stack", &stack_buf);
            log.err("supervisor: task {s} comm={s} wchan={s}\n{s}", .{ ent.name, std.mem.trim(u8, comm, "\n"), wchan, stack });
        }
    }

    fn readTaskFile(io: Io, fp: *[128]u8, task_dir: []const u8, tid: []const u8, name: []const u8, buf: []u8) []const u8 {
        const path = std.fmt.bufPrint(fp, "{s}/{s}/{s}", .{ task_dir, tid, name }) catch return "";
        return Io.Dir.cwd().readFile(io, path, buf) catch "";
    }

    fn readWaiting(io: Io, conn: u64) ?u64 {
        var pb: [96]u8 = undefined;
        var buf: [32]u8 = undefined;
        const path = std.fmt.bufPrint(&pb, "{s}/{d}/waiting", .{ sysfs_connections, conn }) catch return null;
        const text = Io.Dir.cwd().readFile(io, path, &buf) catch return null;
        return parseWaiting(text);
    }

    fn writeAbort(conn: u64) bool {
        var pb: [96:0]u8 = undefined;
        const path = std.fmt.bufPrintSentinel(&pb, "{s}/{d}/abort", .{ sysfs_connections, conn }, 0) catch return false;
        const fd_rc = linux.open(path.ptr, .{ .ACCMODE = .WRONLY, .CLOEXEC = true }, 0);
        if (linux.errno(fd_rc) != .SUCCESS) return false;
        const fd: linux.fd_t = @intCast(fd_rc);
        defer _ = linux.close(fd);
        return linux.errno(linux.write(fd, "1", 1)) == .SUCCESS;
    }

    /// The charter's ladder, once: dump → SIGABRT → (grace) → SIGKILL →
    /// reap → `abort` iff `waiting > 0`.
    pub fn escalate(io: Io, pid: linux.pid_t, conn: ?u64, for_ns: u64, policy: Policy) void {
        log.err("supervisor: daemon {d} unresponsive for {d} s (no answer to stat of the mount root) — dumping state, then SIGABRT → SIGKILL", .{ pid, for_ns / std.time.ns_per_s });
        dumpDaemonState(io, pid);
        _ = linux.kill(pid, .ABRT);
        const deadline = clock.monotonicNs() + ABORT_GRACE_NS;
        var status: i32 = 0;
        var reaped = false;
        while (!reaped and clock.monotonicNs() < deadline) {
            reaped = reap(pid, &status);
            if (!reaped) sleepNs(10 * std.time.ns_per_ms);
        }
        if (!reaped) {
            _ = linux.kill(pid, .KILL);
            while (!reap(pid, &status)) sleepNs(10 * std.time.ns_per_ms);
        }
        log.err("supervisor: daemon {d} is dead (wait status 0x{x})", .{ pid, @as(u32, @bitCast(status)) });
        const c = conn orelse {
            log.err("supervisor: the connection id was unknown at start — nothing to abort; `umount -l` the mountpoint by hand", .{});
            return;
        };
        const waiting = readWaiting(io, c) orelse 0;
        if (policy.write_abort and waiting > 0) {
            if (writeAbort(c)) {
                log.err("supervisor: wrote {s}/{d}/abort ({d} request(s) were waiting): blocked callers get ECONNABORTED; `squeezefs umount` the mountpoint and remount", .{ sysfs_connections, c, waiting });
            } else {
                log.err("supervisor: writing {s}/{d}/abort failed; `umount -l` the mountpoint by hand", .{ sysfs_connections, c });
            }
        } else {
            log.err("supervisor: connection {d} has {d} waiting request(s); no abort written — `squeezefs umount` the mountpoint and remount", .{ c, waiting });
        }
    }

    /// The loop (the intermediate process after `ready`): returns the exit
    /// code — 0 when the daemon exited on its own (a clean umount), 1 after
    /// an escalation.
    pub fn run(gpa: std.mem.Allocator, io: Io, pid: linux.pid_t, mountpoint: [:0]const u8, policy: Policy) u8 {
        var name_buf: thread_name.CommBuf = undefined;
        _ = thread_name.nameCurrentThread(&name_buf, "sqz-supervise");
        const conn = connectionId(mountpoint);
        if (conn) |c| log.info("supervisor: watching daemon {d} at {s} (fuse connection {d}; probe every {d} s, escalate after {d} s)", .{ pid, mountpoint, c, policy.probe_interval_ns / std.time.ns_per_s, policy.unresponsive_after_ns / std.time.ns_per_s }) else log.warn("supervisor: stat({s}) failed at start — the connection id is unknown, the abort rung is disabled", .{mountpoint});
        var state = State.init(clock.monotonicNs());
        var escalated = false;
        while (true) {
            sleepNs(policy.probe_interval_ns);
            var status: i32 = 0;
            if (reap(pid, &status)) {
                log.info("supervisor: daemon {d} exited (wait status 0x{x}); done", .{ pid, @as(u32, @bitCast(status)) });
                return if (escalated) 1 else 0;
            }
            if (!alive(pid)) return if (escalated) 1 else 0;
            const ok = probe(gpa, mountpoint, policy.probe_interval_ns);
            const now = clock.monotonicNs();
            switch (state.observe(now, ok, policy)) {
                .healthy, .escalated => {},
                .recovered => log.warn("supervisor: daemon {d} answers again", .{pid}),
                .grace => |for_ns| log.warn("supervisor: daemon {d} missed a probe ({d} s unresponsive; grace window {d} s)", .{ pid, for_ns / std.time.ns_per_s, policy.unresponsive_after_ns / std.time.ns_per_s }),
                .escalate => |for_ns| {
                    escalated = true;
                    escalate(io, pid, conn, for_ns, policy);
                    return 1;
                },
            }
        }
    }
} else struct {};

// ---------------------------------------------------------------------------
// Tests: the pure machine.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "state machine: escalates exactly once after the threshold; a success re-arms; grace reports the age" {
    const p: Policy = .{};
    var s = State.init(1000);
    try testing.expectEqual(Verdict.healthy, s.observe(1000 + 5 * std.time.ns_per_s, true, p));
    try testing.expectEqual(Verdict{ .grace = 5 * std.time.ns_per_s }, s.observe(1000 + 10 * std.time.ns_per_s, false, p));
    try testing.expectEqual(Verdict{ .grace = 29 * std.time.ns_per_s }, s.observe(1000 + 34 * std.time.ns_per_s, false, p));
    try testing.expectEqual(Verdict{ .escalate = 30 * std.time.ns_per_s }, s.observe(1000 + 35 * std.time.ns_per_s, false, p));
    try testing.expectEqual(Verdict.escalated, s.observe(1000 + 40 * std.time.ns_per_s, false, p));
    try testing.expectEqual(Verdict.escalated, s.observe(1000 + 400 * std.time.ns_per_s, false, p));
    // A success re-arms: recovered, then the window counts from it.
    try testing.expectEqual(Verdict.recovered, s.observe(1000 + 401 * std.time.ns_per_s, true, p));
    try testing.expectEqual(Verdict.healthy, s.observe(1000 + 406 * std.time.ns_per_s, true, p));
    try testing.expectEqual(Verdict{ .grace = 5 * std.time.ns_per_s }, s.observe(1000 + 411 * std.time.ns_per_s, false, p));
    try testing.expectEqual(Verdict{ .escalate = 30 * std.time.ns_per_s }, s.observe(1000 + 436 * std.time.ns_per_s, false, p));
}

test "constants: the probe reaches the daemon past the 1 s attr TTL; six probes to escalation; the abort grace is one lock-plane tick" {
    try testing.expect(PROBE_INTERVAL_NS > std.time.ns_per_s);
    try testing.expectEqual(@as(u64, 6), UNRESPONSIVE_NS / PROBE_INTERVAL_NS);
    try testing.expectEqual(tick_law.tick_ns, ABORT_GRACE_NS);
}

test "waiting parses one decimal line" {
    try testing.expectEqual(@as(?u64, 3), parseWaiting("3\n"));
    try testing.expectEqual(@as(?u64, 0), parseWaiting("0"));
    try testing.expectEqual(@as(?u64, null), parseWaiting(""));
    try testing.expectEqual(@as(?u64, null), parseWaiting("x"));
}

fn waitingFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [256]u8 = undefined;
    const len = smith.slice(&buf);
    _ = parseWaiting(buf[0..len]);
}

test "fuzz: the supervisor's waiting-count parser is total on arbitrary text" {
    try fuzzkit.fuzz({}, waitingFuzz, .{});
}
