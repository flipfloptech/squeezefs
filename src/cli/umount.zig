//! `squeezefs umount [--lazy] <mountpoint>` (charter §8 "`umount`
//! (teardown promotes staged files first)"; §4 "Teardown": only an exit
//! with no teardown or an EXTERNAL `umount(8)` — which sends no DESTROY on
//! a type-`fuse` mount — leaves the kernel's drain as the recourse;
//! decision 0003 §S, the busy-mount contract): the verb SIGNALS the daemon
//! and lets its orderly `close()` run the teardown in the charter's order
//! — the drains release every retained zero-copy slot while the connection
//! is live, the staged files promote, the final checkpoint lands, and the
//! daemon's own `umount2` detaches — then waits for the daemon's exit and
//! confirms the detach. Linux only.
//!
//! The daemon is found through the mount's own `.stats` inode (the
//! `mount` family's `pid`): the one channel that names exactly the process
//! serving THIS mountpoint — and the orderly stop is sent only for the
//! mount the daemon MADE (`isOwnMount`: the family's `mountpoint` and
//! `mount_id`); a bind or a moved mountpoint of the same superblock is
//! the kernel's plain detach, the daemon unsignalled (it serves the mounts
//! of its superblock that remain and exits when the last goes). A mount that serves no document under a LIVE
//! daemon — a FAILED volume whose ops answer EIO, a connection the kernel
//! ended under it (ENOTCONN; fstests generic/475 under dm-error) — is
//! found by its PROCESS instead (`daemonByProcess`: the one `squeezefs
//! mount` whose argv names this mountpoint, its supervisor's seat
//! excluded) and signalled and awaited the same way: a verb that detached
//! and returned with the daemon still tearing down handed the next mount a
//! writer claim its holder had not yet released. When no daemon answers
//! either way (a mount whose daemon already died, a foreign FUSE mount, a
//! path that is no mount) the verb falls back LOUD to the external detach
//! — root's `umount2(path, 0)` with EBUSY retried for one lock-plane tick
//! (`sync/tick.zig`, 2 s: a caller still returning from its last syscall),
//! anyone else's `fusermount3 -u` — never a lazy detach by itself:
//! `MNT_DETACH` hides an owed reply behind a disappearing mountpoint, and
//! the operator asked for a real unmount.
//!
//! BUSY (decision 0003 §S). Before the signal the verb runs the holder
//! census (`holders.zig`: every process's cwd/root/exe/fds/mappings on the
//! mount, by the kernel's mount id; this process `chdir`s to `/` first so
//! it is never its own holder). A held mount is REFUSED: `EBUSY`, the
//! holders named on stderr, exit 1, nothing signalled — the mount keeps
//! serving, as `umount(8)` of a busy ext4 would. The daemon runs the same
//! census when the signal lands (a raw `kill -TERM` meets it too) and
//! refuses the same way, counting `transport_umount_busy_refusals`; the
//! verb reads that counter back through `.stats` so a holder that arrived
//! between the two censuses is still answered `EBUSY` with its name, not
//! a 30 s wait (the daemon's census exempts its stop's sender's fds on
//! the control documents: that very read is no holder). `--lazy` is the explicit opt-in to the other shape: the
//! daemon detaches the mountpoint now (`MNT_DETACH`) and serves every
//! holder until the kernel ends the connection, then tears down; the verb
//! names the holders it leaves behind and waits for the exit only when
//! there are none.
//!
//! THE VERDICT IS ORDER-BASED, on kernel facts alone (never 0 after a
//! kill): the mount's entry gone from the table while the daemon lived (or
//! the daemon gone with the mount detached and no holder left) is the
//! daemon's own unmount — exit 0 once it exited; the daemon gone with the
//! mount still attached, or with holders still on a lazily detached mount,
//! is a death (the supervisor's ladder, a crash) — exit 1, the mountpoint
//! freed externally so the dead connection does not sit there as
//! `ENOTCONN`; a daemon alive past the exit bound is exit 1 and never
//! detached from under. The daemon's life is its THREAD GROUP's, read on a
//! pidfd pinned before the signal (`pin`, the located start ticks re-read
//! on it): a leader that exited first is a zombie while its other threads
//! still hold its files (the writer claim, the devices), and no exit.

const std = @import("std");
const testkit = @import("../core/testkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const cli = @import("cli.zig");
const args = @import("args.zig");
const fusermount = @import("fusermount.zig");
const holders = @import("holders.zig");
const supervisor = @import("supervisor.zig");
const errors = @import("../core/errors.zig");
const clock = @import("../core/clock.zig");
const json_scan = @import("../core/json_scan.zig");
const mountinfo = @import("../core/mountinfo.zig");
const tick_law = @import("../sync/tick.zig");
const transport = @import("../fuse/transport.zig");
const proc = @import("../meta/guard/proc.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("cli/umount is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const BUSY_RETRY_NS: u64 = tick_law.tick_ns;

/// How long the signalled daemon may take to exit: the session close's
/// residency backstop (the longest wait its teardown can owe a retained
/// zero-copy slot) plus the lock plane's tick for the final checkpoint,
/// the dismount pass and the lane's trailing barrier (each sub-second on
/// a quiet mount; the tick is their shared backstop quantum).
pub const DAEMON_EXIT_WAIT_NS: u64 = transport.worker.RESIDENCY_NS + tick_law.tick_ns;

/// The poll quantum of the exit wait (the smallest the daemon's own
/// teardown can be observed at without spinning).
const EXIT_POLL_NS: u64 = 20 * std.time.ns_per_ms;

/// Polls between two reads of the daemon's `.stats` while it still serves
/// (its refusal counter), and between two holder censuses once a lazy
/// detach is seen: a render is a cold document and a census a `/proc`
/// walk, read at the drain's lease budget (100 ms), not at the liveness
/// quantum.
const STATS_POLL_EVERY: u64 = transport.worker.DRAIN_LEASE_BUDGET_NS / EXIT_POLL_NS;

/// Root's path: `umount2(path, 0)` with the bounded EBUSY retry; the
/// errno of the last attempt on failure.
pub fn umountAsRoot(path: [:0]const u8) linux.E {
    const deadline = clock.monotonicNs() + BUSY_RETRY_NS;
    while (true) {
        const e = linux.errno(linux.umount2(path.ptr, 0));
        if (e != .BUSY or clock.monotonicNs() >= deadline) return e;
        std.Thread.yield() catch {};
    }
}

/// The daemon's pid out of the `.stats` document's `mount` family (the
/// first `"pid"` key is the mount's — the family precedes every per-op
/// family). Null when absent or 0 (a document from a renderer that
/// knows no daemon).
pub fn daemonPidOf(stats_text: []const u8) ?linux.pid_t {
    const n = json_scan.number(stats_text, "pid") orelse return null;
    if (n == 0 or n > std.math.maxInt(i32)) return null;
    return @intCast(n);
}

/// The daemon's busy-refusal count out of `.stats` (0 when the family or
/// the key is absent: an older daemon).
pub fn refusalsOf(stats_text: []const u8) u64 {
    return json_scan.number(stats_text, "transport_umount_busy_refusals") orelse 0;
}

/// The id of the mount the daemon made, out of `.stats` (`mount.mount_id`);
/// null when absent or 0 (an older daemon, or one whose mount table was
/// unreadable at `mount(2)`).
pub fn mountIdOf(stats_text: []const u8) ?u64 {
    const id = json_scan.number(stats_text, "mount_id") orelse return null;
    return if (id == 0) null else id;
}

/// Is the mount at `path` — its table entry `entry_id` — the daemon's OWN
/// mount, the one its `mount(2)` made? Every other mount of its superblock
/// (a bind of it or of a path inside it, a bind stacked over its own
/// path, a mountpoint `mount --move`d elsewhere) reads the same `.stats`
/// and names the same pid, but is the KERNEL's to detach: the daemon
/// serves whatever mounts of its superblock remain and exits when the
/// last goes (the kernel ends the connection), and a daemon signalled for
/// a bind unmounted ITS path instead and left the bind dead. Own ⇔ the
/// daemon's mountpoint is this path AND its published mount id is this
/// entry's (a daemon publishing none is judged by the path alone).
pub fn isOwnMount(stats_text: []const u8, entry_id: u64, path: []const u8) bool {
    const mp = json_scan.string(stats_text, "mountpoint") orelse return true;
    if (!std.mem.eql(u8, mp, path)) return false;
    const id = mountIdOf(stats_text) orelse return true;
    return id == entry_id;
}

/// Why the verb fell back to the external detach (each reported by name).
pub const Fallback = enum {
    /// `<mountpoint>/.stats` could not be read and no `squeezefs mount`
    /// process names the path (no mount, a foreign FUSE filesystem, a
    /// daemon that died: the connection is aborted).
    stats_unreadable,
    /// The document names no daemon pid.
    no_pid,
    /// The pid no longer exists (the daemon died between the read and
    /// the signal).
    daemon_gone,

    pub fn text(self: Fallback) []const u8 {
        return switch (self) {
            .stats_unreadable => "its .stats inode is unreadable and no `squeezefs mount` process names it (no squeezefs daemon serves it, or the daemon died)",
            .no_pid => "its .stats names no daemon pid",
            .daemon_gone => "the daemon named by .stats no longer exists",
        };
    }
};

/// The daemon's build commit out of `.stats` (`build.commit`, the first
/// `"commit"` key: the build family precedes every other).
pub fn buildCommitOf(stats_text: []const u8) ?[]const u8 {
    return json_scan.string(stats_text, "commit");
}

/// The signalled path's verdict.
pub const Signalled = union(enum) {
    /// The daemon unmounted (its mount left the table while it lived) and
    /// exited.
    exited: linux.pid_t,
    /// The daemon died with the mount still attached (or, lazily
    /// detached, with holders still on it): a kill or a crash, never its
    /// own unmount.
    died: linux.pid_t,
    /// The daemon refused the stop: the mount is busy (its census ran
    /// after ours and found a holder); it keeps serving.
    busy: linux.pid_t,
    /// `--lazy`: the mount is detached and the daemon serves the holders
    /// left on it (the census that found them — the verb names them from
    /// it, no second walk); it exits when the last leaves.
    detached_held: struct { pid: linux.pid_t, holders: holders.Census },
    /// The daemon is still alive at the deadline (never detached from
    /// under it: its drains may still owe the kernel replies).
    still_alive: linux.pid_t,
    /// `--lazy` on a daemon of another build: SIGUSR2's default action
    /// KILLS a process without our handler, so the lazy stop is sent only
    /// to a daemon that provably has one — the same build as this verb.
    foreign_build: linux.pid_t,
    /// `--lazy` on a daemon found by its process: with no `.stats` its
    /// build cannot be proven, so no SIGUSR2 — and nothing detached from
    /// under a live daemon (the orderly stop is the way out).
    build_unknown: linux.pid_t,
    /// No pidfd could be opened on the daemon (`pin`'s refusal): its
    /// thread group's exit cannot be awaited, so it was not signalled.
    unpinned: struct { pid: linux.pid_t, errno: linux.E },
    fallback: Fallback,
};

/// A located daemon: its pid and its start ticks (`/proc/<pid>/stat`
/// field 22) read at the locate — together its identity for this boot, so
/// a pid freed and reissued before the pin is another process (`pin`).
pub const Daemon = struct { pid: linux.pid_t, start_ticks: u64 };

/// Where the daemon of a mountpoint was found (`locateDaemon`).
pub const Located = union(enum) {
    /// The `.stats` document (owned by the caller) and the daemon it names.
    stats: struct { daemon: Daemon, text: []u8 },
    /// No document; the daemon by its process (`daemonByProcess`).
    process: Daemon,
    fallback: Fallback,
};

/// The daemon serving `mountpoint`: by its `.stats` (the channel that
/// names exactly this mount's process), else by its process (module doc).
pub fn locateDaemon(gpa: std.mem.Allocator, io: Io, mountpoint: []const u8) Located {
    if (readStats(gpa, io, mountpoint)) |text| {
        const pid = daemonPidOf(text) orelse {
            gpa.free(text);
            return .{ .fallback = .no_pid };
        };
        const ticks = startTicksOf(io, pid) orelse {
            gpa.free(text);
            return .{ .fallback = .daemon_gone };
        };
        return .{ .stats = .{ .daemon = .{ .pid = pid, .start_ticks = ticks }, .text = text } };
    }
    if (daemonByProcess(io, mountpoint)) |pid| {
        const ticks = startTicksOf(io, pid) orelse return .{ .fallback = .daemon_gone };
        return .{ .process = .{ .pid = pid, .start_ticks = ticks } };
    }
    return .{ .fallback = .stats_unreadable };
}

/// The start time of `pid` (`/proc/<pid>/stat` field 22, clock ticks since
/// boot) in ANY state: a zombie leader's still names it, and its pid cannot
/// be reissued while it stands. Null when unreadable (no such process).
fn startTicksOf(io: Io, pid: linux.pid_t) ?u64 {
    var pb: [64]u8 = undefined;
    var buf: [1024]u8 = undefined;
    const path = std.fmt.bufPrint(&pb, "/proc/{d}/stat", .{pid}) catch return null;
    const text = Io.Dir.cwd().readFile(io, path, &buf) catch return null;
    return statStartTicks(text);
}

/// Field 22 of a `/proc/<pid>/stat` line. The comm (field 2) is
/// parenthesised and may hold spaces and parens, so the count resumes
/// after the LAST ')' at field 3.
fn statStartTicks(text: []const u8) ?u64 {
    const close = std.mem.lastIndexOfScalar(u8, text, ')') orelse return null;
    var it = std.mem.tokenizeAny(u8, text[close + 1 ..], " \n");
    var field: usize = 3;
    while (it.next()) |tok| : (field += 1) {
        if (field == 22) return std.fmt.parseInt(u64, tok, 10) catch null; // decimal, as proc(5) prints it
    }
    return null;
}

/// The located daemon pinned by a pidfd: a signal through it reaches
/// exactly that process, and its exit is its whole thread group's.
const Pinned = struct {
    fd: linux.fd_t,

    /// Has the whole thread group exited? A pidfd reads POLLIN once the
    /// leader has exited AND no thread of its group is left (`pidfd_poll`):
    /// a leader that exited first is a zombie in `/proc/<pid>/stat` while
    /// its other threads still run and hold the fd table — the writer
    /// claim, the devices — and its pidfd is not readable until the last of
    /// them is gone (they release the files before that: `do_exit` runs
    /// `exit_files` before `exit_notify`). Then a zombie, a task its
    /// parent is reaping ('X') and a released process all read exited,
    /// for good. A poll that fails reads as not yet: the wait runs on to
    /// its bound.
    fn exited(self: Pinned) bool {
        var pfd = [1]linux.pollfd{.{ .fd = self.fd, .events = linux.POLL.IN, .revents = 0 }};
        const rc = linux.poll(&pfd, 1, 0);
        return linux.errno(rc) == .SUCCESS and rc == 1 and pfd[0].revents & linux.POLL.IN != 0;
    }

    fn close(self: Pinned) void {
        _ = linux.close(self.fd);
    }
};

const PinOutcome = union(enum) {
    pinned: Pinned,
    /// Gone already (ESRCH), or the pid now names another process (its
    /// start ticks differ from the located ones).
    gone,
    /// `pidfd_open` refused with this errno: EINVAL and ENOSYS cannot
    /// happen on the kernel floor (≥ 7.3; pidfd_open is 5.3's), ENOENT is a
    /// thread's id (no process's), EMFILE/ENFILE/ENOMEM/ENODEV a resource.
    refused: linux.E,
};

/// Pin the located daemon: a pidfd on its pid, the start ticks re-read
/// after the open equal to the located ones (the process then lived from
/// the locate through the open — a pid is never reissued while its process
/// stands — so the pidfd is its).
fn pin(io: Io, d: Daemon) PinOutcome {
    const rc = linux.pidfd_open(d.pid, 0);
    switch (linux.errno(rc)) {
        .SUCCESS => {},
        .SRCH => return .gone,
        else => |e| return .{ .refused = e },
    }
    const p: Pinned = .{ .fd = @intCast(rc) };
    if (startTicksOf(io, d.pid) != d.start_ticks) {
        p.close();
        return .gone;
    }
    return .{ .pinned = p };
}

/// A `squeezefs mount` process naming a mountpoint (`daemonByProcess`):
/// its parent, and whether it has a child of its own (the supervisor's
/// seat has the daemon — alive, or a zombie it has not reaped yet; a daemon
/// keeps no child).
const Candidate = struct {
    pid: linux.pid_t,
    ppid: linux.pid_t,
    has_children: bool = false,
    /// Its comm is the supervisor's (`supervisor.thread_comm`): a seat that
    /// outlived its daemon holding its custody (a failed replay's tail, a
    /// write cache for the successor) — childless, yet never the daemon.
    supervisor: bool = false,
};

/// At most this many `squeezefs mount` processes naming ONE path are
/// looked at: the daemon and its supervisor's seat are two; more is a
/// bring-up racing the stop, which is never a guess.
const CANDIDATES_MAX = 4;

/// Does a `/proc/<pid>/cmdline` document (NUL-separated argv) run the
/// mount verb at `mountpoint`? Parsed by the verb's own argument parser, so
/// every flag shape `squeezefs mount` accepts is read the way it was.
pub fn cmdlineMounts(cmdline: []const u8, mountpoint: []const u8) bool {
    var argv: [32][]const u8 = undefined;
    var n: usize = 0;
    var words = std.mem.splitScalar(u8, cmdline, 0);
    while (words.next()) |w| {
        if (w.len == 0) continue;
        if (n == argv.len) return false;
        argv[n] = w;
        n += 1;
    }
    var diag: args.Refusal = undefined;
    const verb = args.parse(argv[0..n], &diag) catch return false;
    return switch (verb) {
        .mount => |m| std.mem.eql(u8, m.mountpoint, mountpoint),
        else => false,
    };
}

/// The one daemon among the processes naming a mountpoint: a candidate
/// that is another candidate's parent, or has any child, is the
/// supervisor's seat (the intermediate of the double fork shares the
/// daemon's argv and holds the daemon as its child — a zombie still, once
/// the daemon exited and the seat has not reaped it; a TERM would end the
/// seat by default action) and is dropped; exactly one left is the daemon,
/// anything else is null.
pub fn pickDaemon(cands: []const Candidate) ?linux.pid_t {
    var picked: ?linux.pid_t = null;
    for (cands) |c| {
        if (c.supervisor or c.has_children or parentAmong(cands, c.pid)) continue;
        if (picked != null) return null;
        picked = c.pid;
    }
    return picked;
}

fn parentAmong(cands: []const Candidate, pid: linux.pid_t) bool {
    for (cands) |c| if (c.ppid == pid) return true;
    return false;
}

/// `PPid:\t<n>` out of a `/proc/<pid>/status` document.
fn parseStatusPpid(text: []const u8) ?linux.pid_t {
    var lines = std.mem.splitScalar(u8, text, '\n');
    while (lines.next()) |line| {
        if (!std.mem.startsWith(u8, line, "PPid:")) continue;
        const v = std.mem.trim(u8, line["PPid:".len..], " \t\r");
        return std.fmt.parseInt(linux.pid_t, v, 10) catch null; // decimal, as proc(5) prints it
    }
    return null;
}

/// The daemon of `mountpoint` by its PROCESS, for a mount that serves no
/// `.stats` document under a live daemon (a FAILED volume whose ops
/// answer EIO; a connection the kernel ended under it, ENOTCONN): the one
/// `squeezefs mount` process whose argv names this path (`cmdlineMounts`),
/// its supervisor's seat excluded (`pickDaemon`). Null when none — a dead
/// daemon, and the detach is right — or more than one.
pub fn daemonByProcess(io: Io, mountpoint: []const u8) ?linux.pid_t {
    var cands: [CANDIDATES_MAX]Candidate = undefined;
    var n: usize = 0;
    // Two PATH_MAX paths, the options and the flags fit; a longer argv is
    // another program's.
    var cmdline_buf: [16 << 10]u8 = undefined;
    var proc_dir = Io.Dir.cwd().openDir(io, "/proc", .{ .iterate = true }) catch return null;
    defer proc_dir.close(io);
    var it = proc_dir.iterate();
    while (it.next(io) catch null) |ent| {
        const pid = std.fmt.parseInt(linux.pid_t, ent.name, 10) catch continue; // a pid directory, decimal
        if (pid == linux.getpid()) continue;
        var pb: [64]u8 = undefined;
        const cmdline_path = std.fmt.bufPrint(&pb, "/proc/{d}/cmdline", .{pid}) catch continue;
        // A proc file reports size 0, so it is read into a buffer, never
        // sized by stat (`readFileAlloc` answers it empty); argv past the
        // buffer is no `squeezefs mount` line.
        const cmdline = Io.Dir.cwd().readFile(io, cmdline_path, &cmdline_buf) catch continue;
        if (!cmdlineMounts(cmdline, mountpoint)) continue;
        if (n == cands.len) return null;
        var sb: [4096]u8 = undefined;
        const status_path = std.fmt.bufPrint(&pb, "/proc/{d}/status", .{pid}) catch continue;
        const status = Io.Dir.cwd().readFile(io, status_path, &sb) catch continue;
        // `children` (CONFIG_PROC_CHILDREN) lists the pids the task holds,
        // zombies included; a kernel without it reads as none and the
        // parent rule alone tells the seat apart.
        const children_path = std.fmt.bufPrint(&pb, "/proc/{d}/task/{d}/children", .{ pid, pid }) catch continue;
        var cb: [64]u8 = undefined;
        const children = Io.Dir.cwd().readFile(io, children_path, &cb) catch "";
        const comm_path = std.fmt.bufPrint(&pb, "/proc/{d}/comm", .{pid}) catch continue;
        var commb: [32]u8 = undefined;
        const comm = Io.Dir.cwd().readFile(io, comm_path, &commb) catch "";
        cands[n] = .{ .pid = pid, .ppid = parseStatusPpid(status) orelse 0, .has_children = std.mem.trim(u8, children, " \n").len != 0, .supervisor = std.mem.eql(u8, std.mem.trim(u8, comm, " \n"), supervisor.thread_comm) };
        n += 1;
    }
    return pickDaemon(cands[0..n]);
}

/// Test seam: fires at the top of each poll of the exit wait, before its
/// liveness read — the threaded-exit pin holds the leader-first window
/// across polls.
var test_exit_poll: if (builtin.is_test) ?PollHook else void = if (builtin.is_test) null else {};
const PollHook = struct { ctx: *anyopaque, fire: *const fn (ctx: *anyopaque, pid: linux.pid_t) void };

/// `<mountpoint>/.stats` (owned); null when unreadable. The 16 MiB bound
/// is the renderer's own document cap.
fn readStats(gpa: std.mem.Allocator, io: Io, mountpoint: []const u8) ?[]u8 {
    const path = std.fmt.allocPrint(gpa, "{s}/.stats", .{mountpoint}) catch return null;
    defer gpa.free(path);
    return Io.Dir.cwd().readFileAlloc(io, path, gpa, .limited(16 << 20)) catch null;
}

/// The mount under judgement: its table entry at the start (null when
/// nothing is mounted at the path), the census over it, the mount id the
/// verdict watches.
const Watched = struct {
    gpa: std.mem.Allocator,
    io: Io,
    mountpoint: []const u8,
    target: ?holders.Target = null,
    /// Two table buffers: the start's (the target and its stacked mounts
    /// slice into it) and a scratch for every later look.
    table: []u8,
    scratch: []u8,
    text: []const u8 = "",

    fn init(gpa: std.mem.Allocator, io: Io, mountpoint: []const u8) Watched {
        var self: Watched = .{ .gpa = gpa, .io = io, .mountpoint = mountpoint, .table = &.{}, .scratch = &.{} };
        self.table = gpa.alloc(u8, mountinfo.READ_BYTES) catch return self;
        self.scratch = gpa.alloc(u8, mountinfo.READ_BYTES) catch return self;
        self.text = mountinfo.read(io, self.table);
        if (mountinfo.entryAt(self.text, mountpoint)) |e| self.target = holders.Target.of(e);
        return self;
    }

    fn deinit(self: *Watched) void {
        if (self.scratch.len != 0) self.gpa.free(self.scratch);
        if (self.table.len != 0) self.gpa.free(self.table);
    }

    /// Is the mount we started with still attached at the path?
    fn attached(self: *const Watched) bool {
        const t = self.target orelse return false;
        if (self.scratch.len == 0) return false;
        const e = mountinfo.entryAt(mountinfo.read(self.io, self.scratch), self.mountpoint) orelse return false;
        return e.id == t.mount_id;
    }

    /// The census over the mount (attached or lazily detached: its holders
    /// still reference its id), everyone but this verb and the daemon.
    fn census(self: *const Watched, daemon_pid: linux.pid_t) holders.Census {
        const t = self.target orelse return .{};
        return holders.census(self.gpa, self.io, t, self.text, &.{ linux.getpid(), daemon_pid }, null);
    }

    /// `census` once the mount LEFT the table (the lazy verdict's holders
    /// left behind): the kernel reissues a freed mount id to the next
    /// mount anywhere on the host, so the id is trusted only while no
    /// ATTACHED mount carries it — a detached mount never returns to the
    /// table, and its id stays taken while a holder references it, so an
    /// attached mount under our id means ours is gone with every holder,
    /// and whatever the census would name is that other mount's.
    fn censusDetached(self: *const Watched, daemon_pid: linux.pid_t) holders.Census {
        const t = self.target orelse return .{};
        if (self.scratch.len == 0) return .{};
        if (mountinfo.hasId(mountinfo.read(self.io, self.scratch), t.mount_id)) return .{};
        return self.census(daemon_pid);
    }

    /// The lazy verdict once the daemon is gone and the mount has left the
    /// table: did it die with holders still on the detached superblock, or
    /// did the kernel end its connection first (the orderly exit)? The
    /// CONNECTION is the fact, not a holder census: a freed mount id is
    /// reissued to the next mount on the host — to a lazily detached one
    /// too, which no table shows — so the census named another mount's
    /// holders and an orderly `umount -l` under the gate's parallel shards
    /// read "died without unmounting". With fusectl mounted the connection
    /// lives exactly while `connections/<minor>` stands (the superblock's
    /// kill removes it, and a superblock with holders is not killed), and
    /// a reissued minor is an attached mount's (`hasDev`). Without fusectl
    /// the census stands, guarded by the id's reissue to an attached mount.
    fn diedWithHolders(self: *const Watched, daemon_pid: linux.pid_t) bool {
        const t = self.target orelse return false;
        if (self.scratch.len == 0) return false;
        var pb: [128]u8 = undefined;
        if (Io.Dir.cwd().access(self.io, transport.session.fusectl_connections, .{})) |_| {
            const conn = std.fmt.bufPrint(&pb, "{s}/{d}", .{ transport.session.fusectl_connections, t.dev_minor }) catch return false;
            Io.Dir.cwd().access(self.io, conn, .{}) catch return false;
            return !mountinfo.hasDev(mountinfo.read(self.io, self.scratch), t.dev_major, t.dev_minor);
        } else |_| {}
        return self.censusDetached(daemon_pid).busy();
    }
};

/// Signal the daemon serving `mountpoint` (TERM, or USR2 under `lazy`)
/// and judge the outcome by the order of the kernel's facts (module doc).
pub fn signalDaemon(gpa: std.mem.Allocator, io: Io, mountpoint: []const u8, lazy: bool, our_commit: []const u8, watched: *const Watched) Signalled {
    return signalLocated(gpa, io, mountpoint, lazy, our_commit, watched, locateDaemon(gpa, io, mountpoint));
}

/// `signalDaemon` over a daemon already located (`run` reports how it was
/// found before the wait). Takes the document's ownership.
pub fn signalLocated(gpa: std.mem.Allocator, io: Io, mountpoint: []const u8, lazy: bool, our_commit: []const u8, watched: *const Watched, located: Located) Signalled {
    // The refusal count the busy re-read compares against; null when the
    // mount serves no document (then its census' refusal is not readable
    // either, and the verb's own census already ran).
    var refusals_before: ?u64 = null;
    const daemon: Daemon = switch (located) {
        .fallback => |why| return .{ .fallback = why },
        .process => |d| if (lazy) return .{ .build_unknown = d.pid } else d,
        .stats => |s| blk: {
            defer gpa.free(s.text);
            if (lazy and !std.mem.eql(u8, buildCommitOf(s.text) orelse "", our_commit)) return .{ .foreign_build = s.daemon.pid };
            refusals_before = refusalsOf(s.text);
            break :blk s.daemon;
        },
    };
    const pid = daemon.pid;
    const pinned = switch (pin(io, daemon)) {
        .pinned => |p| p,
        .gone => return .{ .fallback = .daemon_gone },
        .refused => |e| return .{ .unpinned = .{ .pid = pid, .errno = e } },
    };
    defer pinned.close();
    if (linux.errno(linux.pidfd_send_signal(pinned.fd, if (lazy) .USR2 else .TERM, null, 0)) != .SUCCESS) return .{ .fallback = .daemon_gone };
    const deadline = clock.monotonicNs() + DAEMON_EXIT_WAIT_NS;
    var detached = false;
    var polls: u64 = 0;
    while (true) : (polls += 1) {
        if (builtin.is_test) if (test_exit_poll) |h| h.fire(h.ctx, pid);
        const is_alive = !pinned.exited();
        // The cold reads run at the document cadence — and the census at
        // the poll that first sees the detach: the verb's "at once".
        var cold_due = polls % STATS_POLL_EVERY == 0;
        if (!detached and !watched.attached()) {
            detached = true;
            cold_due = true;
        }
        if (!is_alive) {
            if (!detached) return .{ .died = pid };
            // Detached and gone: the daemon's own unmount, unless it left
            // holders behind on a lazily detached mount (then it died
            // before the kernel ended the connection).
            if (lazy and watched.diedWithHolders(pid)) return .{ .died = pid };
            return .{ .exited = pid };
        }
        if (detached) {
            if (lazy and cold_due) {
                const c = watched.censusDetached(pid);
                if (c.busy()) return .{ .detached_held = .{ .pid = pid, .holders = c } };
            }
        } else if (!lazy and cold_due) {
            // Still attached and serving: did the daemon's census refuse?
            if (refusals_before) |before| if (readStats(gpa, io, mountpoint)) |now_text| {
                defer gpa.free(now_text);
                if (refusalsOf(now_text) > before) return .{ .busy = pid };
            };
        }
        if (clock.monotonicNs() >= deadline) return .{ .still_alive = pid };
        io.sleep(.fromNanoseconds(EXIT_POLL_NS), .awake) catch {};
    }
}

/// Is `path` (still) a `fuse`-type mount? `/proc/self/mountinfo`'s exact
/// mountpoint field (never a stat of the path: that is a request to the
/// very daemon whose exit is being judged).
pub fn isFuseMount(io: Io, gpa: std.mem.Allocator, path: []const u8) bool {
    const buf = gpa.alloc(u8, mountinfo.READ_BYTES) catch return true;
    defer gpa.free(buf);
    return mountinfoIsFuse(mountinfo.read(io, buf), path);
}

/// The fstype of the LAST line naming `mountpoint` is `fuse`/`fuse.*`.
pub fn mountinfoIsFuse(text: []const u8, mountpoint: []const u8) bool {
    const e = mountinfo.entryAt(text, mountpoint) orelse return false;
    return e.isFuse();
}

/// The external detach (the fallback): root's `umount2`, else fusermount3.
fn detachExternally(p: cli.Process, path: [:0]const u8) u8 {
    if (linux.geteuid() == 0) {
        const e = umountAsRoot(path);
        if (e == .SUCCESS) return cli.exit_ok;
        p.stderr.print("squeezefs umount: umount2({s}) failed: {t} ({s})\n", .{ path, e, @errorName(errors.fromErrno(e)) }) catch {};
        return cli.exit_failed;
    }
    if (fusermount.unmount(p.io, path)) return cli.exit_ok;
    p.stderr.print("squeezefs umount: fusermount3 -u {s} failed\n", .{path}) catch {};
    return cli.exit_failed;
}

/// The kernel's detach of a mount that is not the daemon's own (a bind, a
/// move): root's `umount2` — plain with the bounded EBUSY retry, or
/// `MNT_DETACH` under `--lazy` — else fusermount3; the daemon is not
/// signalled.
fn detachOther(p: cli.Process, path: [:0]const u8, lazy: bool) u8 {
    if (linux.geteuid() == 0) {
        const e = if (lazy) linux.errno(linux.umount2(path.ptr, linux.MNT.DETACH)) else umountAsRoot(path);
        if (e == .SUCCESS) return cli.exit_ok;
        p.stderr.print("squeezefs umount: umount2({s}) failed: {t} ({s})\n", .{ path, e, @errorName(errors.fromErrno(e)) }) catch {};
        return cli.exit_failed;
    }
    if (if (lazy) fusermount.unmountLazy(p.io, path) else fusermount.unmount(p.io, path)) return cli.exit_ok;
    p.stderr.print("squeezefs umount: fusermount3 -u {s} failed\n", .{path}) catch {};
    return cli.exit_failed;
}

/// The refusal line: the typed class (`error.Busy`, EBUSY through the ONE
/// table) and the holders.
fn refuseBusy(p: cli.Process, path: []const u8, c: holders.Census) u8 {
    p.stderr.print("squeezefs umount: {s} is busy ({f}) — {t} ({s}): nothing unmounted, the mount keeps serving; retry when they are gone, or `squeezefs umount --lazy {s}` to detach now and let the daemon serve them to the end\n", .{ path, c, errors.toErrno(error.Busy), @errorName(error.Busy), path }) catch {};
    return cli.exit_failed;
}

pub fn run(p: cli.Process, u: args.Umount) u8 {
    var zbuf: [linux.PATH_MAX:0]u8 = undefined;
    const path = std.fmt.bufPrintSentinel(&zbuf, "{s}", .{u.mountpoint}, 0) catch {
        p.stderr.writeAll("squeezefs umount: the mountpoint path is too long\n") catch {};
        return cli.exit_usage;
    };
    // Never our own holder: the verb is often typed from inside the mount.
    _ = linux.chdir("/");
    var watched = Watched.init(p.gpa, p.io, path);
    defer watched.deinit();
    // The pre-signal census: a daemon pid is unknown yet (its `.stats` is
    // read by `signalDaemon`), and the daemon holds nothing of its mount.
    if (!u.lazy and watched.target != null) {
        const c = watched.census(0);
        if (c.busy()) return refuseBusy(p, path, c);
    }
    const located = locateDaemon(p.gpa, p.io, path);
    // No entry at the path in a table that was read, and no daemon names
    // it: nothing to unmount (a second umount of one path, generic/753's
    // `_cleanup`) — never the dead-daemon fallback's external detach.
    if (located == .fallback and watched.target == null and watched.text.len != 0) {
        p.stderr.print("squeezefs umount: {s}: not mounted\n", .{path}) catch {};
        return cli.exit_failed;
    }
    if (located == .process) p.stderr.print("squeezefs umount: {s}: its .stats inode is unreadable (its volume is FAILED, or the kernel ended its connection) but daemon {d} still serves it — signalling it and waiting for its exit\n", .{ path, located.process.pid }) catch {};
    // Another mountpoint of the daemon's superblock (`isOwnMount`): the
    // kernel's detach, nothing signalled; the daemon serves what remains.
    if (located == .stats) if (watched.target) |t| if (!isOwnMount(located.stats.text, t.mount_id, path)) {
        defer p.gpa.free(located.stats.text);
        p.stderr.print("squeezefs umount: {s} is a bind or a moved mountpoint of daemon {d}'s mount at {s}, not the mount it made — detached {s} without signalling the daemon, which serves the mounts of its superblock that remain and exits when the last goes\n", .{ path, located.stats.daemon.pid, json_scan.string(located.stats.text, "mountpoint") orelse "?", if (u.lazy) "lazily" else "plainly" }) catch {};
        return detachOther(p, path, u.lazy);
    };
    switch (signalLocated(p.gpa, p.io, path, u.lazy, p.build.commit, &watched, located)) {
        .exited => |pid| {
            if (!isFuseMount(p.io, p.gpa, path)) return cli.exit_ok;
            // A successor mounted over the path after ours left: not ours
            // to touch; the daemon's own cycle completed.
            p.stderr.print("squeezefs umount: daemon {d} exited and its mount is gone; another FUSE mount now sits at {s} (left alone)\n", .{ pid, path }) catch {};
            return cli.exit_ok;
        },
        .busy => |pid| return refuseBusy(p, path, watched.census(pid)),
        .detached_held => |d| {
            p.stdout.print("squeezefs umount: {s} detached; daemon {d} serves the {f} until the last leaves, then tears down (its supervisor logs the exit)\n", .{ path, d.pid, d.holders }) catch {};
            return cli.exit_ok;
        },
        .died => |pid| {
            p.stderr.print("squeezefs umount: daemon {d} died without unmounting {s} (see its log and supervisor) — the dead connection is detached externally; the unmount was NOT orderly\n", .{ pid, path }) catch {};
            if (isFuseMount(p.io, p.gpa, path)) _ = detachExternally(p, path);
            return cli.exit_failed;
        },
        .still_alive => |pid| {
            p.stderr.print("squeezefs umount: daemon {d} has not exited {d} s after the signal — its teardown is still running (or wedged: see its log and supervisor); not detaching from under it\n", .{ pid, DAEMON_EXIT_WAIT_NS / std.time.ns_per_s }) catch {};
            return cli.exit_failed;
        },
        .foreign_build => |pid| {
            p.stderr.print("squeezefs umount: daemon {d} at {s} is another build than this verb — the lazy stop (SIGUSR2) is sent only to a daemon known to handle it; `squeezefs umount {s}` (the orderly stop) works across builds\n", .{ pid, path, path }) catch {};
            return cli.exit_failed;
        },
        .build_unknown => |pid| {
            p.stderr.print("squeezefs umount: daemon {d} at {s} serves no .stats (its volume is FAILED, or the kernel ended its connection), so its build cannot be proven — the lazy stop (SIGUSR2) is sent only to a daemon known to handle it; nothing detached from under it: `squeezefs umount {s}` (the orderly stop) signals it and waits for its exit\n", .{ pid, path, path }) catch {};
            return cli.exit_failed;
        },
        .unpinned => |d| {
            p.stderr.print("squeezefs umount: daemon {d} at {s}: pidfd_open refused it ({t}, {s}) — the exit of its every thread cannot be awaited, so it was NOT signalled and nothing is unmounted\n", .{ d.pid, path, d.errno, @errorName(errors.fromErrno(d.errno)) }) catch {};
            return cli.exit_failed;
        },
        .fallback => |why| {
            p.stderr.print("squeezefs umount: {s}: {s} — detaching externally (an external unmount of a type-fuse mount sends the daemon no DESTROY; a still-running daemon tears down on the connection's abort)\n", .{ path, why.text() }) catch {};
            const rc = detachExternally(p, path);
            if (rc != cli.exit_ok and watched.target != null) {
                const c = watched.census(0);
                if (c.busy()) p.stderr.print("squeezefs umount: {s} is held by {f}\n", .{ path, c }) catch {};
            }
            return rc;
        },
    }
}

const testing = std.testing;

test "the daemon pid is the mount family's `pid`; 0 or absent names no daemon; the refusal count defaults to 0; the build commit is the build family's" {
    try testing.expectEqual(@as(?linux.pid_t, 4242), daemonPidOf("{\"build\":{},\"mount\":{\"uid\":0,\"gid\":0,\"pid\":4242,\"mountpoint\":\"/m\"}}"));
    try testing.expectEqual(@as(?linux.pid_t, null), daemonPidOf("{\"mount\":{\"uid\":0,\"pid\":0}}"));
    try testing.expectEqual(@as(?linux.pid_t, null), daemonPidOf("{\"mount\":{\"uid\":0}}"));
    try testing.expectEqual(@as(?linux.pid_t, null), daemonPidOf("{\"mount\":{\"pid\":4294967296}}"));
    try testing.expectEqual(@as(u64, 0), refusalsOf("{\"mount\":{\"pid\":1}}"));
    try testing.expectEqualStrings("abc123", buildCommitOf("{\"build\":{\"train\":\"0.1.0\",\"commit\":\"abc123\",\"profile\":\"gate\"},\"mount\":{\"pid\":1}}").?);
    try testing.expect(buildCommitOf("{\"mount\":{\"pid\":1}}") == null);
    try testing.expectEqual(@as(u64, 3), refusalsOf("{\"transport\":{\"transport_umount_busy_retries\":9,\"transport_umount_busy_refusals\":3}}"));
}

test "the daemon's OWN mount is the one at its mountpoint carrying its published mount id; a bind elsewhere, a bind stacked over its path and a moved mountpoint are not; a daemon publishing no id (0, absent) is judged by the path alone" {
    const doc = "{\"build\":{},\"mount\":{\"uid\":0,\"gid\":0,\"pid\":4242,\"mountpoint\":\"/mnt/a\",\"mount_id\":90,\"sqmeta_uri\":\"sqmeta:///dev/m\"}}";
    try testing.expectEqual(@as(?u64, 90), mountIdOf(doc));
    try testing.expect(isOwnMount(doc, 90, "/mnt/a"));
    try testing.expect(!isOwnMount(doc, 93, "/mnt/b")); // a bind (or a move) elsewhere
    try testing.expect(!isOwnMount(doc, 93, "/mnt/a")); // a bind stacked over the daemon's path
    try testing.expect(!isOwnMount(doc, 90, "/mnt/b")); // the daemon's mount moved: the kernel's detach ends the connection
    const no_id = "{\"mount\":{\"pid\":4242,\"mountpoint\":\"/mnt/a\",\"mount_id\":0}}";
    try testing.expectEqual(@as(?u64, null), mountIdOf(no_id));
    try testing.expect(isOwnMount(no_id, 93, "/mnt/a"));
    try testing.expect(!isOwnMount(no_id, 93, "/mnt/b"));
    try testing.expect(isOwnMount("{\"mount\":{\"pid\":4242,\"mountpoint\":\"/mnt/a\"}}", 1, "/mnt/a"));
    try testing.expect(isOwnMount("{\"mount\":{\"pid\":4242}}", 1, "/mnt/a"));
}

test "the exit wait covers the workers' residency backstop plus one tick; the .stats poll rides the drain's lease budget" {
    try testing.expectEqual(transport.worker.RESIDENCY_NS + tick_law.tick_ns, DAEMON_EXIT_WAIT_NS);
    try testing.expect(DAEMON_EXIT_WAIT_NS > transport.worker.RESIDENCY_NS);
    try testing.expectEqual(@as(u64, 5), STATS_POLL_EVERY);
}

test "mountinfo: a fuse-type line at the exact mountpoint, the last line wins, a prefix match or another fstype is not a FUSE mount" {
    const text =
        \\22 1 0:21 / / rw,relatime - ext4 /dev/root rw
        \\90 22 0:45 / /build/tmp/mnt-7 rw,nosuid,nodev,relatime - fuse squeezefs rw,user_id=0,group_id=0
        \\91 22 0:46 / /build/tmp/mnt-77 rw,relatime - fuse.sshfs x rw
        \\92 22 0:47 / /build/tmp/mnt-8 rw,relatime - tmpfs tmpfs rw
        \\
    ;
    try testing.expect(mountinfoIsFuse(text, "/build/tmp/mnt-7"));
    try testing.expect(mountinfoIsFuse(text, "/build/tmp/mnt-77"));
    try testing.expect(!mountinfoIsFuse(text, "/build/tmp/mnt-8"));
    try testing.expect(!mountinfoIsFuse(text, "/build/tmp/mnt"));
    try testing.expect(!mountinfoIsFuse(text, "/"));
    // A later line over the same path shadows the earlier mount.
    const shadowed = text ++ "93 22 0:48 / /build/tmp/mnt-7 rw,relatime - tmpfs tmpfs rw\n";
    try testing.expect(!mountinfoIsFuse(shadowed, "/build/tmp/mnt-7"));
}

test "a directory that is no mount: no daemon to signal (.stats unreadable), not watched, not a FUSE mount, and the external detach reports the kernel's EINVAL through the ONE table" {
    if (linux.geteuid() != 0) return testkit.skip(@src(), .no_root, "umount2(2) of a non-mount needs root");
    _ = linux.mkdir("/build/tmp", 0o755);
    var pb: [64:0]u8 = undefined;
    const dir = try std.fmt.bufPrintSentinel(&pb, "/build/tmp/not-a-mount-{d}", .{linux.getpid()}, 0);
    _ = linux.mkdir(dir.ptr, 0o755);
    defer _ = linux.rmdir(dir.ptr);
    var watched = Watched.init(testing.allocator, testing.io, dir);
    defer watched.deinit();
    try testing.expect(watched.target == null);
    try testing.expect(!watched.attached());
    try testing.expectEqual(Signalled{ .fallback = .stats_unreadable }, signalDaemon(testing.allocator, testing.io, dir, false, "", &watched));
    try testing.expect(!isFuseMount(testing.io, testing.allocator, dir));
    const e = umountAsRoot(dir);
    try testing.expectEqual(linux.E.INVAL, e);
    try testing.expectEqual(error.InvalidArgument, errors.fromErrno(e));
}

test "the process census: a cmdline is the daemon's iff the mount verb's parser reads it as a mount of exactly this path; among the candidates a parent of another is the supervisor's seat, one left is the daemon, two unrelated are never a guess" {
    const line = "squeezefs\x00mount\x00--daemon\x00--log-file\x00/var/log/sqz.log\x00-o\x00default_permissions,allow_other\x00sqmeta:///dev/mapper/meta\x00/mnt/scratch\x00";
    try testing.expect(cmdlineMounts(line, "/mnt/scratch"));
    try testing.expect(!cmdlineMounts(line, "/mnt/scratch2"));
    try testing.expect(!cmdlineMounts(line, "/mnt"));
    try testing.expect(!cmdlineMounts("squeezefs\x00umount\x00/mnt/scratch\x00", "/mnt/scratch"));
    try testing.expect(!cmdlineMounts("squeezefs\x00mount\x00/mnt/scratch\x00", "/mnt/scratch"));
    try testing.expect(!cmdlineMounts("", "/mnt/scratch"));
    try testing.expectEqual(@as(?linux.pid_t, 41), parseStatusPpid("Name:\tsqueezefs\nState:\tS (sleeping)\nTgid:\t42\nPid:\t42\nPPid:\t41\n"));
    try testing.expectEqual(@as(?linux.pid_t, null), parseStatusPpid("Name:\tsqueezefs\n"));
    try testing.expectEqual(@as(?linux.pid_t, null), pickDaemon(&.{}));
    try testing.expectEqual(@as(?linux.pid_t, 42), pickDaemon(&.{.{ .pid = 42, .ppid = 1 }}));
    // The supervisor's seat (the daemon's parent, the same argv) is dropped.
    try testing.expectEqual(@as(?linux.pid_t, 42), pickDaemon(&.{ .{ .pid = 41, .ppid = 1 }, .{ .pid = 42, .ppid = 41 } }));
    try testing.expectEqual(@as(?linux.pid_t, 42), pickDaemon(&.{ .{ .pid = 42, .ppid = 41 }, .{ .pid = 41, .ppid = 1 } }));
    try testing.expectEqual(@as(?linux.pid_t, 42), pickDaemon(&.{ .{ .pid = 41, .ppid = 1, .has_children = true }, .{ .pid = 42, .ppid = 41 } }));
    // The seat alone — its daemon exited and sits unreaped as its zombie
    // child (no argv of its own): nobody to signal, never the seat.
    try testing.expectEqual(@as(?linux.pid_t, null), pickDaemon(&.{.{ .pid = 41, .ppid = 1, .has_children = true }}));
    // The seat alone after it reaped its daemon, holding its custody (a
    // failed replay's tail, a write cache): childless, named by its comm —
    // never the daemon (a TERM would end its hold).
    try testing.expectEqual(@as(?linux.pid_t, null), pickDaemon(&.{.{ .pid = 41, .ppid = 1, .supervisor = true }}));
    try testing.expectEqual(@as(?linux.pid_t, 52), pickDaemon(&.{ .{ .pid = 41, .ppid = 1, .supervisor = true }, .{ .pid = 52, .ppid = 51 }, .{ .pid = 51, .ppid = 1, .has_children = true } }));
    // Two daemons naming one path (a bring-up racing the stop): no guess.
    try testing.expectEqual(@as(?linux.pid_t, null), pickDaemon(&.{ .{ .pid = 42, .ppid = 1 }, .{ .pid = 52, .ppid = 1 } }));
    try testing.expectEqual(@as(?linux.pid_t, null), pickDaemon(&.{ .{ .pid = 41, .ppid = 1 }, .{ .pid = 42, .ppid = 41 }, .{ .pid = 52, .ppid = 41 } }));
}

/// The stand-in daemon of the process-census test: `/bin/bash` exec'd under
/// the daemon's own argv (`squeezefs mount … <dir>`), running the script
/// `mount` in `dir` (a shell's command file is its first word), which
/// names its pid on stdout and parks on stdin — so `/proc/<pid>/cmdline`
/// is a daemon's line without a mount. Bash, never `/bin/sh`: bash's
/// SIGCHLD handler reaps the seat's daemon (a child it never started) on
/// every venue, where dash leaves it a zombie.
fn standInDaemon(dir: [*:0]const u8, out_fd: linux.fd_t, in_fd: linux.fd_t) noreturn {
    _ = linux.chdir(dir);
    _ = linux.dup2(in_fd, 0);
    _ = linux.dup2(out_fd, 1);
    const argv = [_:null]?[*:0]const u8{ "squeezefs", "mount", "--daemon", "--log-file", "/l", "-o", "rw", "sqmeta:///dev/x", dir };
    const envp = [_:null]?[*:0]const u8{};
    _ = linux.execve("/bin/bash", &argv, &envp);
    linux.exit(127);
}

/// The tests' liveness: the verdict's own observable, a pidfd's readiness
/// (`Pinned.exited`), on a fresh pidfd — alive while any thread of the
/// group runs. An open refused other than by ESRCH reads alive (never a
/// false gone).
fn alive(pid: linux.pid_t) bool {
    const rc = linux.pidfd_open(pid, 0);
    switch (linux.errno(rc)) {
        .SUCCESS => {},
        .SRCH => return false,
        else => return true,
    }
    const p: Pinned = .{ .fd = @intCast(rc) };
    defer p.close();
    return !p.exited();
}

test "a mount serving no .stats under a LIVE daemon (a FAILED volume's EIO; a connection the kernel ended — fstests generic/475's soak loop, where the verb detached and returned with the daemon still tearing down and the next mount met its claim): the verb finds the daemon by its process — the supervisor's seat sharing the argv excluded — signals it and waits for its exit; `--lazy` refuses with the build unknown and signals nothing; a path no process names keeps the stats_unreadable fallback" {
    const io = testing.io;
    const gpa = testing.allocator;
    _ = linux.mkdir("/build/tmp", 0o755);
    var pb: [64]u8 = undefined;
    const dir = try std.fmt.bufPrint(&pb, "/build/tmp/failed-mount-{d}", .{linux.getpid()});
    var zb: [64:0]u8 = undefined;
    const dirz = try std.fmt.bufPrintSentinel(&zb, "{s}", .{dir}, 0);
    _ = linux.mkdir(dirz.ptr, 0o755);
    defer Io.Dir.cwd().deleteTree(io, dir) catch {};
    var sp: [96]u8 = undefined;
    try Io.Dir.cwd().writeFile(io, .{ .sub_path = try std.fmt.bufPrint(&sp, "{s}/mount", .{dir}), .data = "echo $$\nread x\n" });
    var ready: [2]linux.fd_t = undefined;
    var park: [2]linux.fd_t = undefined;
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.pipe2(&ready, .{ .CLOEXEC = true })));
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.pipe2(&park, .{ .CLOEXEC = true })));
    // The supervisor's seat forks the daemon, then takes the same argv.
    const rc = linux.fork();
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc));
    if (rc == 0) {
        if (linux.fork() == 0) standInDaemon(dirz.ptr, ready[1], park[0]);
        standInDaemon(dirz.ptr, ready[1], park[0]);
    }
    const seat: linux.pid_t = @intCast(rc);
    _ = linux.close(ready[1]);
    _ = linux.close(park[0]);
    defer {
        // The seat leaves at EOF; its zombie child goes to init with it.
        _ = linux.close(park[1]);
        var status: i32 = 0;
        _ = linux.waitpid(seat, &status, 0);
    }
    // Two pid lines, either order; the one that is not the seat is the daemon.
    var lines: [64]u8 = undefined;
    var got: usize = 0;
    while (std.mem.count(u8, lines[0..got], "\n") < 2) {
        const n = linux.read(ready[0], lines[got..].ptr, lines.len - got);
        if (linux.errno(n) != .SUCCESS or n == 0) break;
        got += n;
    }
    _ = linux.close(ready[0]);
    var daemon: ?linux.pid_t = null;
    var it = std.mem.tokenizeScalar(u8, lines[0..got], '\n');
    while (it.next()) |l| {
        const pid = try std.fmt.parseInt(linux.pid_t, l, 10);
        if (pid != seat) daemon = pid;
    }
    try testing.expect(daemon != null);
    var watched = Watched.init(gpa, io, dir);
    defer watched.deinit();
    try testing.expect(watched.target == null);
    try testing.expectEqual(daemon, daemonByProcess(io, dir));
    try testing.expectEqual(@as(?linux.pid_t, null), daemonByProcess(io, "/build/tmp/no-process-mounts-this"));
    try testing.expectEqual(Located{ .process = .{ .pid = daemon.?, .start_ticks = startTicksOf(io, daemon.?).? } }, locateDaemon(gpa, io, dir));
    try testing.expectEqual(Signalled{ .fallback = .stats_unreadable }, signalDaemon(gpa, io, "/build/tmp/no-process-mounts-this", false, "", &watched));
    // --lazy: no document, no proof of the build, no SIGUSR2 — the daemon lives on.
    try testing.expectEqual(Signalled{ .build_unknown = daemon.? }, signalDaemon(gpa, io, dir, true, "", &watched));
    try testing.expect(alive(daemon.?));
    // The seat stopped: it holds its daemon's zombie until the reap below.
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.kill(seat, .STOP)));
    defer _ = linux.kill(seat, .CONT);
    var stop_status: i32 = 0;
    try testing.expectEqual(@as(usize, @intCast(seat)), linux.waitpid(seat, &stop_status, linux.W.UNTRACED));
    try testing.expect(linux.W.IFSTOPPED(@bitCast(stop_status)));
    // The orderly stop: TERM ends the stand-in (its default action) and the
    // verdict waits for that exit — the daemon's, never the seat's.
    try testing.expectEqual(Signalled{ .exited = daemon.? }, signalDaemon(gpa, io, dir, false, "", &watched));
    // A daemon gone stays gone: the zombie the stopped seat holds, then the
    // released pid once the seat continues and its SIGCHLD handler reaps it
    // (a full gate's shard once read that reap as a live daemon, §CO.37).
    try testing.expect(!alive(daemon.?));
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.kill(seat, .CONT)));
    while (linux.errno(linux.kill(daemon.?, @fromBackingInt(@intCast(0)))) != .SRCH) std.Thread.yield() catch {};
    try testing.expect(!alive(daemon.?));
    try testing.expect(alive(seat));
    // (The seat left alone with its daemon as an unreaped zombie — the
    // supervisor between the daemon's exit and its next probe — is the
    // pure census' row above.)
}

test "the located identity and the pin: field 22 of /proc/<pid>/stat in any state (the comm may hold spaces and parens); the pin is a pidfd whose start ticks re-read after the open are the located ones — a reissued pid (other ticks) and a reaped one are gone, a thread's id is no process's and is refused by its errno with nothing signalled; a pinned live process has not exited" {
    const io = testing.io;
    const gpa = testing.allocator;
    try testing.expectEqual(@as(?u64, 987654), statStartTicks("42 (sqz w) x) Z 41 42 42 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 987654 0 0\n"));
    try testing.expectEqual(@as(?u64, null), statStartTicks("42 (sqz) S 41 42\n"));
    try testing.expectEqual(@as(?u64, null), statStartTicks("no comm"));
    const me = linux.getpid();
    const ticks = startTicksOf(io, me).?;
    switch (pin(io, .{ .pid = me, .start_ticks = ticks })) {
        .pinned => |p| {
            defer p.close();
            try testing.expect(!p.exited());
        },
        else => return error.TestUnexpectedResult,
    }
    try testing.expect(pin(io, .{ .pid = me, .start_ticks = ticks + 1 }) == .gone);
    // A reaped pid: a fork's child, waited.
    const rc = linux.fork();
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc));
    if (rc == 0) linux.exit(0);
    const dead: linux.pid_t = @intCast(rc);
    var status: i32 = 0;
    _ = linux.waitpid(dead, &status, 0);
    try testing.expect(pin(io, .{ .pid = dead, .start_ticks = 0 }) == .gone);
    // A thread's id: ENOENT (`pidfd_prepare`: no thread-group linkage).
    var tp: [2]linux.fd_t = undefined;
    var hp: [2]linux.fd_t = undefined;
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.pipe2(&tp, .{ .CLOEXEC = true })));
    defer for (tp) |fd| {
        _ = linux.close(fd);
    };
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.pipe2(&hp, .{ .CLOEXEC = true })));
    defer _ = linux.close(hp[0]);
    const Parked = struct {
        fn main(out: linux.fd_t, hold: linux.fd_t) void {
            const t = linux.gettid();
            _ = linux.write(out, @ptrCast(&t), @sizeOf(linux.pid_t));
            var b: u8 = 0;
            _ = linux.read(hold, @ptrCast(&b), 1);
        }
    };
    const th = try std.Thread.spawn(.{}, Parked.main, .{ tp[1], hp[0] });
    defer {
        _ = linux.close(hp[1]);
        th.join();
    }
    var tid: linux.pid_t = 0;
    try testing.expectEqual(@as(usize, @sizeOf(linux.pid_t)), linux.read(tp[0], @ptrCast(&tid), @sizeOf(linux.pid_t)));
    const thread: Daemon = .{ .pid = tid, .start_ticks = startTicksOf(io, tid).? };
    try testing.expectEqual(PinOutcome{ .refused = .NOENT }, pin(io, thread));
    var watched = Watched.init(gpa, io, "/build/tmp/no-mount-here");
    defer watched.deinit();
    try testing.expectEqual(Signalled{ .unpinned = .{ .pid = tid, .errno = .NOENT } }, signalLocated(gpa, io, "/build/tmp/no-mount-here", false, "", &watched, .{ .process = thread }));
}

test "a .stats document naming a dead pid falls back as `daemon_gone`" {
    _ = linux.mkdir("/build/tmp", 0o755);
    var pb: [64]u8 = undefined;
    const dir = try std.fmt.bufPrint(&pb, "/build/tmp/fake-mount-{d}", .{linux.getpid()});
    var zb: [64:0]u8 = undefined;
    const dirz = try std.fmt.bufPrintSentinel(&zb, "{s}", .{dir}, 0);
    _ = linux.mkdir(dirz.ptr, 0o755);
    defer Io.Dir.cwd().deleteTree(testing.io, dir) catch {};
    // A pid no process holds: the kernel's pid space ends at
    // /proc/sys/kernel/pid_max (≤ 4194304); a child reaped by its parent
    // long ago is not guaranteed free, so a freshly forked-and-reaped pid
    // is the one certainly-dead number.
    const rc = linux.fork();
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc));
    if (rc == 0) linux.exit(0);
    const dead: linux.pid_t = @intCast(rc);
    var status: i32 = 0;
    _ = linux.waitpid(dead, &status, 0);
    var sb: [96]u8 = undefined;
    const path = try std.fmt.bufPrint(&sb, "{s}/.stats", .{dir});
    var doc: [128]u8 = undefined;
    const text = try std.fmt.bufPrint(&doc, "{{\"mount\":{{\"uid\":0,\"pid\":{d}}}}}", .{dead});
    try Io.Dir.cwd().writeFile(testing.io, .{ .sub_path = path, .data = text });
    var watched = Watched.init(testing.allocator, testing.io, dir);
    defer watched.deinit();
    try testing.expectEqual(Signalled{ .fallback = .daemon_gone }, signalDaemon(testing.allocator, testing.io, dir, false, "", &watched));
}

/// The threaded-exit pin's hold, at each poll of the exit wait: the first
/// poll that finds the stand-in's leader a zombie records the window (the
/// claim still held; the daemon read alive or not); the next poll releases
/// the second thread, so only a wait for the whole group reaches the
/// verdict after the release.
const LeaderFirst = struct {
    stand_in: *const proc.ThreadedStandIn,
    lock_fd: linux.fd_t,
    seen: bool = false,
    window_held: bool = false,
    window_alive: bool = false,
    released: bool = false,

    fn fire(ctx: *anyopaque, pid: linux.pid_t) void {
        const self: *LeaderFirst = @ptrCast(@alignCast(ctx));
        if (pid != self.stand_in.pid or self.released) return;
        if (self.seen) {
            self.released = self.stand_in.release();
            return;
        }
        if (proc.stateLetter(pid) != 'Z') return;
        self.seen = true;
        self.window_held = linux.errno(linux.flock(self.lock_fd, std.posix.LOCK.EX | std.posix.LOCK.NB)) == .AGAIN;
        self.window_alive = alive(pid);
    }
};

test "a daemon whose MAIN thread exits first (a teardown's exit_group: the leader a zombie while its other threads still run and hold its fds — the writer claim, the devices): the verdict waits for the whole thread group, so at `.exited` the daemon's files are released and the next mount meets no claim of its" {
    const io = testing.io;
    const gpa = testing.allocator;
    _ = linux.mkdir("/build/tmp", 0o755);
    var pb: [64]u8 = undefined;
    const dir = try std.fmt.bufPrint(&pb, "/build/tmp/threaded-exit-{d}", .{linux.getpid()});
    var zb: [64:0]u8 = undefined;
    const dirz = try std.fmt.bufPrintSentinel(&zb, "{s}", .{dir}, 0);
    _ = linux.mkdir(dirz.ptr, 0o755);
    defer Io.Dir.cwd().deleteTree(io, dir) catch {};
    var lb: [96:0]u8 = undefined;
    const lock_path = try std.fmt.bufPrintSentinel(&lb, "{s}/claim", .{dir}, 0);
    var stand_in = try proc.ThreadedStandIn.start(lock_path.ptr);
    defer stand_in.deinit();
    const daemon = stand_in.pid;
    // Our own open of the claim: refused while the stand-in holds it.
    const lrc = linux.open(lock_path.ptr, .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(lrc));
    const lock_fd: linux.fd_t = @intCast(lrc);
    defer _ = linux.close(lock_fd);
    try testing.expectEqual(linux.E.AGAIN, linux.errno(linux.flock(lock_fd, std.posix.LOCK.EX | std.posix.LOCK.NB)));
    // The document names the stand-in: a daemon found by its `.stats`.
    var sb: [96]u8 = undefined;
    var doc: [64]u8 = undefined;
    try Io.Dir.cwd().writeFile(io, .{ .sub_path = try std.fmt.bufPrint(&sb, "{s}/.stats", .{dir}), .data = try std.fmt.bufPrint(&doc, "{{\"mount\":{{\"uid\":0,\"pid\":{d}}}}}", .{daemon}) });
    var watched = Watched.init(gpa, io, dir);
    defer watched.deinit();
    var hold: LeaderFirst = .{ .stand_in = &stand_in, .lock_fd = lock_fd };
    test_exit_poll = .{ .ctx = &hold, .fire = &LeaderFirst.fire };
    defer test_exit_poll = null;
    try testing.expectEqual(Signalled{ .exited = daemon }, signalDaemon(gpa, io, dir, false, "", &watched));
    // The law: at the verdict the daemon's files are released.
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.flock(lock_fd, std.posix.LOCK.EX | std.posix.LOCK.NB)));
    // The window was held: a poll met the leader a zombie with the claim
    // still held and read the daemon alive; the release preceded the verdict.
    try testing.expect(hold.window_held);
    try testing.expect(hold.window_alive);
    try testing.expect(hold.released);
}

test "a tmpfs held by a child's cwd: the pre-signal census refuses it by name (the verb's law), the plain umount2 is EBUSY, and after the child leaves nobody holds it" {
    if (linux.geteuid() != 0) return testkit.skip(@src(), .no_root, "mount(2) of a tmpfs needs root");
    const io = testing.io;
    const gpa = testing.allocator;
    _ = linux.mkdir("/build/tmp", 0o755);
    var pb: [64:0]u8 = undefined;
    const mp = try std.fmt.bufPrintSentinel(&pb, "/build/tmp/umount-busy-{d}", .{linux.getpid()}, 0);
    _ = linux.mkdir(mp.ptr, 0o755);
    defer _ = linux.rmdir(mp.ptr);
    if (linux.errno(linux.mount("tmpfs", mp.ptr, "tmpfs", 0, 0)) != .SUCCESS) return testkit.skip(@src(), .no_cap, "mount(2) of a tmpfs refused (CAP_SYS_ADMIN)");
    defer _ = linux.umount2(mp.ptr, linux.MNT.DETACH);
    var ready: [2]linux.fd_t = undefined;
    var done: [2]linux.fd_t = undefined;
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.pipe2(&ready, .{ .CLOEXEC = true })));
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.pipe2(&done, .{ .CLOEXEC = true })));
    const rc = linux.fork();
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc));
    if (rc == 0) {
        _ = linux.close(ready[0]);
        _ = linux.close(done[1]);
        // Readable by the census under a hardened (non-dumpable) test
        // process without CAP_SYS_PTRACE (`holders.zig`'s row says why).
        _ = linux.prctl(@backingInt(linux.PR.SET_DUMPABLE), 1, 0, 0, 0);
        _ = linux.chdir(mp.ptr);
        var one: u8 = 1;
        _ = linux.write(ready[1], @ptrCast(&one), 1);
        _ = linux.close(ready[1]);
        _ = linux.read(done[0], @ptrCast(&one), 1);
        linux.exit(0);
    }
    const child: linux.pid_t = @intCast(rc);
    _ = linux.close(ready[1]);
    var byte: u8 = 0;
    _ = linux.read(ready[0], @ptrCast(&byte), 1);
    _ = linux.close(ready[0]);
    {
        var watched = Watched.init(gpa, io, mp);
        defer watched.deinit();
        try testing.expect(watched.target != null);
        try testing.expect(watched.attached());
        const c = watched.census(0);
        try testing.expectEqual(@as(usize, 1), c.total);
        try testing.expectEqual(child, c.named[0].pid);
        try testing.expectEqual(linux.E.BUSY, linux.errno(linux.umount2(mp.ptr, 0)));
        // The refusal line names the class and the holder.
        var out: [512]u8 = undefined;
        var w: Io.Writer = .fixed(&out);
        try w.print("{s} is busy ({f}) — {t} ({s})", .{ mp, c, errors.toErrno(error.Busy), @errorName(error.Busy) });
        try testing.expect(std.mem.indexOf(u8, w.buffered(), "1 holder(s): pid ") != null);
        try testing.expect(std.mem.indexOf(u8, w.buffered(), ") cwd) — BUSY (Busy)") != null);
    }
    _ = linux.close(done[1]);
    _ = linux.close(done[0]);
    var status: i32 = 0;
    _ = linux.waitpid(child, &status, 0);
    {
        var watched = Watched.init(gpa, io, mp);
        defer watched.deinit();
        try testing.expect(!watched.census(0).busy());
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.umount2(mp.ptr, 0)));
        try testing.expect(!watched.attached());
    }
}
