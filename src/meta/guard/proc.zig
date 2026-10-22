//! The guard's host readers (Linux): the local writer identity (boot id,
//! pid, `/proc/self/stat` start ticks, hostname), the dead-pid prover over
//! `/proc/<pid>/stat`, and the `/etc/nvme/{hostnqn,hostid}` defaults —
//! READ ONLY (quirk Q7: a mount never mints or writes host configuration;
//! an absent file reads as empty).
//!
//! ## Layer A's wait law (`flockExclusive`)
//!
//! A mount whose `flock(LOCK_EX | LOCK_NB)` is refused judges the HOLDER
//! each round, on kernel facts alone (never a request to it: a read of
//! its `.stats` would be an fd on its mount — a HOLDER to its stop's
//! census, and EBUSY to its own `umount2`):
//!
//! - not this binary (udev's block-device probes): waited out for
//!   `SHARED_LOCK_WAIT_NS`, then refused (the row below);
//! - this binary and EXITING — an orderly unmount in progress — waited
//!   out for `EXITING_HOLDER_WAIT_NS` (the orderly-exit ceiling), then
//!   refused. Exiting is one of (`Exiting`): the lock's creator pid is
//!   gone or a zombie while the lock stands (its supervisor's replay
//!   holds the guard's open file description — charter §5.2 "a
//!   successor's `flock` waits out the replay"); the pid is alive and
//!   its mount (its argv's mountpoint) has no FUSE entry in the mount
//!   table (its own `umount2` landed and the dismount pass runs — or it
//!   is still bringing up, which resolves to the next case within its
//!   own bound); the pid is alive and attached, and a `squeezefs umount
//!   <its mountpoint>` stands in the process table (the orderly stop's
//!   verb, alive from before its census to the daemon's exit — the
//!   analogue of the kernel's `s_umount`, which `umount(2)` takes at its
//!   start and `mount(2)` serializes behind; xfstests generic/604);
//! - this binary and LIVE (alive, attached, no verb in flight): refused
//!   AT ONCE — the single-writer law. A holder that is no mount verb (an
//!   offline verb's door) is live too.
//!
//! What this does not see: a holder stopped by a raw `kill -TERM` with
//! its mount still attached (its census and drains, sub-second) — a mount
//! racing that window refuses as before; once its mount leaves the table
//! the wait applies.

const std = @import("std");
const fuzzkit = @import("../../core/fuzzkit.zig");
const testkit = @import("../../core/testkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const Io = std.Io;
const claim = @import("claim.zig");
const clock = @import("../../core/clock.zig");
const counters = @import("../../core/counters.zig");
const log = @import("../../core/log.zig");
const mountinfo = @import("../../core/mountinfo.zig");
const tick_law = @import("../../sync/tick.zig");
const args = @import("../../cli/args.zig");
const umount_verb = @import("../../cli/umount.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("guard/proc is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

/// Read a small file whole with raw syscalls (control path at open and
/// every 10 s; procfs reports size 0 so the read is buffer-driven).
pub fn readSmall(path: [*:0]const u8, buf: []u8) ?[]const u8 {
    const rc = linux.open(path, .{ .ACCMODE = .RDONLY, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return null;
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    var total: usize = 0;
    while (total < buf.len) {
        const n = linux.read(fd, buf.ptr + total, buf.len - total);
        if (linux.errno(n) != .SUCCESS) return null;
        if (n == 0) break;
        total += n;
    }
    return buf[0..total];
}

/// `/proc/sys/kernel/random/boot_id` as its 16 uuid bytes; zero when
/// unreadable or malformed (the gate treats zero as "proves nothing").
pub fn bootId() [16]u8 {
    var buf: [64]u8 = undefined;
    const text = readSmall("/proc/sys/kernel/random/boot_id", &buf) orelse return @splat(0);
    return parseUuid(std.mem.trim(u8, text, " \t\r\n")) orelse @splat(0);
}

/// `8-4-4-4-12` hex, or 32 bare hex digits.
pub fn parseUuid(text: []const u8) ?[16]u8 {
    var out: [16]u8 = undefined;
    var n: usize = 0;
    var hi: ?u8 = null;
    for (text) |ch| {
        if (ch == '-') continue;
        const v: u8 = switch (ch) {
            '0'...'9' => ch - '0',
            'a'...'f' => ch - 'a' + 10,
            'A'...'F' => ch - 'A' + 10,
            else => return null,
        };
        if (hi) |h| {
            if (n == 16) return null;
            out[n] = (h << 4) | v;
            n += 1;
            hi = null;
        } else hi = v;
    }
    if (hi != null or n != 16) return null;
    return out;
}

/// Field 22 of `/proc/<pid>/stat` (start time, clock ticks since boot).
/// The comm field is parenthesised and may hold spaces, so the parse
/// resumes after the LAST ')'. Null = no such process (or unreadable —
/// which the proof reads the same way: nothing alive answers to the pid),
/// and a ZOMBIE or dead task (state `Z`/`X`: exited, its parent has not
/// reaped it — a supervisor's probe interval away): it holds no fd and
/// writes nothing, and its pid cannot be reused while it stands.
pub fn startTicksOf(pid: u32) ?u64 {
    var pb: [64]u8 = undefined;
    const path = std.fmt.bufPrintSentinel(&pb, "/proc/{d}/stat", .{pid}, 0) catch return null;
    var buf: [1024]u8 = undefined;
    const text = readSmall(path.ptr, &buf) orelse return null;
    return parseStartTicks(text);
}

pub fn parseStartTicks(stat: []const u8) ?u64 {
    const close = std.mem.lastIndexOfScalar(u8, stat, ')') orelse return null;
    if (close + 2 > stat.len) return null;
    // After ") " comes field 3 (state); field 22 is 19 fields later.
    var it = std.mem.tokenizeScalar(u8, stat[close + 2 ..], ' ');
    var idx: usize = 3;
    while (it.next()) |tok| : (idx += 1) {
        if (idx == 3 and (std.mem.eql(u8, tok, "Z") or std.mem.eql(u8, tok, "X"))) return null; // field 3 is the state letter (proc(5))
        if (idx == 22) return std.fmt.parseInt(u64, tok, 10) catch null;
    }
    return null;
}

fn startTicksFn(_: ?*anyopaque, pid: u32) ?u64 {
    return startTicksOf(pid);
}

/// The ONE prover (`claim.PidProbe`) over procfs.
pub const probe: claim.PidProbe = .{ .ctx = null, .startTicksOf = startTicksFn };

/// This process's identity: boot id, pid, its own start ticks (0 when
/// `/proc/self/stat` is unreadable — then no later incarnation could ever
/// match it, which errs towards "alive"), the hostname's hash.
pub fn localIdentity() claim.Identity {
    var hb: [std.posix.HOST_NAME_MAX]u8 = undefined;
    const host = std.posix.gethostname(&hb) catch "";
    return .{
        .boot_id = bootId(),
        .pid = @intCast(linux.getpid()),
        .start_ticks = startTicksOf(@intCast(linux.getpid())) orelse 0,
        .hostname_hash = claim.hostnameHash(host),
    };
}

/// `/proc/locks` is a table of every lock on the host; a flock line reads
/// `N: FLOCK  ADVISORY  WRITE pid MAJ:MIN:INO start end`. The table of a
/// busy host is a few hundred lines; 1 MiB holds thousands.
const PROC_LOCKS_CAP: usize = 1 << 20;

/// Is a writer's exclusive `flock` (the D0 guard's layer A) held on `path`
/// on this host — read off `/proc/locks`, so the probe takes NO lock of its
/// own (a `-o ro` reader "claims nothing", and a transient `LOCK_SH` would
/// refuse a writer's `LOCK_EX | LOCK_NB` racing it). A locker that is not
/// this binary (udev's re-read after a write-close, below) is waited out
/// like `flockExclusive` does; one that outlasts the bound reads as held.
/// Unreadable procfs reads as "no lock" (the claim face judges then).
/// `waits` counts each retry a locker caused.
pub fn writerFlockHeld(io: Io, path: []const u8, waits: ?*counters.ShardedCounter) bool {
    const deadline = clock.monotonicNs() + SHARED_LOCK_WAIT_NS;
    while (true) {
        const h = exclusiveLocker(path) orelse return false;
        if (h.squeezefs or clock.monotonicNs() >= deadline) return true;
        if (waits) |w| w.inc();
        io.sleep(.fromMilliseconds(SHARED_LOCK_RETRY_MS), .awake) catch return true;
    }
}

/// The exclusive `flock` locker of `path`'s inode right now (one scan, no
/// wait); null = none, or `path` / `/proc/locks` unreadable.
pub fn exclusiveLocker(path: []const u8) ?Holder {
    var zbuf: [linux.PATH_MAX]u8 = undefined;
    const z = std.fmt.bufPrintSentinel(&zbuf, "{s}", .{path}, 0) catch return null;
    var st: linux.Statx = undefined;
    if (linux.errno(linux.statx(linux.AT.FDCWD, z, 0, .{ .INO = true, .MNT_ID = true }, &st)) != .SUCCESS) return null;
    return lockerOf(&st) catch null;
}

/// systemd-udevd locks a whole-disk block device while it handles it
/// (systemd.io/BLOCK_DEVICE_LOCKING): `LOCK_SH` for an event's rules (a fresh
/// namespace's `add`, a blkid probe), and `LOCK_EX` around the `BLKRRPART`
/// its inotify watch issues after EVERY write-close of the node — a `format`,
/// a config verb, a mount's teardown (a worker's `flock(LOCK_EX|LOCK_NB)`,
/// then the ioctl: ~10 ms idle; strace on kamrui, systemd 259). A squeezefs
/// holder holds for its whole life, so a refused `LOCK_EX | LOCK_NB` is
/// retried unless the exclusive holder is this binary. The bound: one
/// event's rules are a blkid probe (milliseconds); 5 s covers a queue of
/// them behind a storm of new namespaces — past it the refusal stands.
pub const SHARED_LOCK_WAIT_NS: u64 = 5 * std.time.ns_per_s;
/// The retry step: a probe's own scale (one blkid read), so the wait ends
/// within a step of the release.
pub const SHARED_LOCK_RETRY_MS: i64 = 5;

/// The orderly-exit ceiling a mount waits out behind an EXITING holder of
/// this binary (module doc): the umount verb's own bound on the signalled
/// daemon's exit (`cli/umount.DAEMON_EXIT_WAIT_NS`: the session close's
/// residency backstop plus one tick for the final checkpoint, the
/// dismount pass and the lane's trailing barrier) plus two ticks — one
/// for the verb's pre-signal census and `.stats` read (the wait may begin
/// before the signal), one for the supervisor's replay after the exit
/// (the un-landed windows of at most one flush interval; sub-second) —
/// past it the holder is wedged and the refusal stands.
pub const EXITING_HOLDER_WAIT_NS: u64 = umount_verb.DAEMON_EXIT_WAIT_NS + 2 * tick_law.tick_ns;
/// The exiting wait's retry step: the umount verb's own exit-poll quantum
/// (the smallest a teardown is observed at without spinning); each round
/// re-judges the holder, so a stop the census refused is seen within one.
pub const EXITING_RETRY_MS: i64 = 20;

/// The retry counters `flockExclusive` feeds: one per class of holder
/// waited out (each a mechanism's engagement gauge).
pub const Waits = struct {
    /// A holder that is not this binary (udev).
    shared: ?*counters.ShardedCounter = null,
    /// An exiting holder of this binary.
    exiting: ?*counters.ShardedCounter = null,
};

/// `flock(fd, LOCK_EX | LOCK_NB)` that waits out udev's holders and the
/// EXITING holders of this binary (module doc): `.SUCCESS`, `.AGAIN` when
/// a LIVE holder of this binary has it (at once) or a waited-out holder
/// outlasts its bound, or the call's other errno. `waits` counts each
/// retry a holder caused; `locker` is the exclusive locker last seen, with
/// its last judgement (for the refusal's text, `writeLocker`).
pub fn flockExclusive(io: Io, fd: linux.fd_t, waits: Waits, locker: *?Holder) linux.E {
    const start = clock.monotonicNs();
    var st: linux.Statx = undefined;
    // The exiting holder named once per wait (the release line pairs it).
    var waiting_on: ?linux.pid_t = null;
    while (true) {
        const e = linux.errno(linux.flock(fd, std.posix.LOCK.EX | std.posix.LOCK.NB));
        if (e == .SUCCESS) {
            if (waiting_on) |pid| log.info("writer lock: pid {d} released it after {d} ms — proceeding", .{ pid, (clock.monotonicNs() - start) / std.time.ns_per_ms });
            return e;
        }
        if (e != .AGAIN) return e;
        if (linux.errno(linux.statx(fd, "", linux.AT.EMPTY_PATH, .{ .INO = true, .MNT_ID = true }, &st)) != .SUCCESS) return .AGAIN;
        // Unreadable `/proc/locks`: the refusal stands.
        var h = (lockerOf(&st) catch return .AGAIN) orelse Holder{ .pid = 0, .squeezefs = false };
        if (h.pid != 0) {
            h.exiting = judgeExiting(io, h.pid, h.squeezefs);
            locker.* = h;
        }
        const now = clock.monotonicNs();
        if (h.exiting) |why| {
            if (now >= start + EXITING_HOLDER_WAIT_NS) return .AGAIN;
            if (waiting_on != h.pid) {
                waiting_on = h.pid;
                log.info("writer lock: {f} holds it and is exiting ({s}) — waiting for its release (the orderly-exit ceiling, {d} s)", .{ h, why.text(), EXITING_HOLDER_WAIT_NS / std.time.ns_per_s });
            }
            if (waits.exiting) |w| w.inc();
            io.sleep(.fromMilliseconds(EXITING_RETRY_MS), .awake) catch return .AGAIN;
            continue;
        }
        if (h.squeezefs) return .AGAIN;
        if (now >= start + SHARED_LOCK_WAIT_NS) return .AGAIN;
        if (waits.shared) |w| w.inc();
        io.sleep(.fromMilliseconds(SHARED_LOCK_RETRY_MS), .awake) catch return .AGAIN;
    }
}

/// Why a holder of this binary reads as EXITING (module doc).
pub const Exiting = enum {
    /// The lock's creator pid is gone or a zombie while the lock stands:
    /// an inheritor of its open file description holds it — the
    /// supervisor's replay (charter §5.2).
    replay,
    /// Alive, a `squeezefs mount` whose mountpoint has no FUSE entry in
    /// the mount table: its own `umount2` landed and its dismount pass
    /// runs (or it is still bringing up).
    detached,
    /// Alive and attached, with a `squeezefs umount <mountpoint>` in the
    /// process table: the orderly stop's verb.
    umount_in_flight,

    pub fn text(self: Exiting) []const u8 {
        return switch (self) {
            .replay => "its process is gone and its supervisor holds the lock through the journal tail's replay",
            .detached => "its mount has left the mount table and its dismount pass runs",
            .umount_in_flight => "a `squeezefs umount` of its mountpoint is in flight",
        };
    }
};

/// The one judgement of a locker (module doc): null = not exiting (a LIVE
/// holder of this binary, or a foreign one). A creator that is gone or a
/// zombie while its lock stands is the replay whatever its exe read as
/// (a dead pid's `/proc/<pid>/exe` is unreadable, so `squeezefs` is false
/// for every dead creator); a live foreign locker is never judged.
pub fn judgeExiting(io: Io, pid: linux.pid_t, squeezefs: bool) ?Exiting {
    if (startTicksOf(@intCast(pid)) == null) return .replay;
    if (!squeezefs) return null;
    var mp_buf: [linux.PATH_MAX]u8 = undefined;
    const mountpoint = mountpointOfPid(pid, &mp_buf) orelse return null;
    if (!fuseAttached(io, mountpoint)) return .detached;
    return if (umountInFlight(io, mountpoint)) .umount_in_flight else null;
}

/// Is a FUSE mount attached at exactly `mountpoint` (the mount table, as
/// the umount verb reads it — never a stat of the path)?
fn fuseAttached(io: Io, mountpoint: []const u8) bool {
    var buf: [mountinfo.READ_BYTES]u8 = undefined;
    const e = mountinfo.entryAt(mountinfo.read(io, &buf), mountpoint) orelse return false;
    return e.isFuse();
}

/// The mount verb's argv (NUL-separated, as `/proc/<pid>/cmdline` prints
/// it) parsed by the CLI's own grammar; a relative mountpoint, a mount
/// verb's `--help`, another verb: null.
pub fn mountpointOfCmdline(cmdline: []const u8) ?[]const u8 {
    const verb = verbOfCmdline(cmdline) orelse return null;
    return switch (verb) {
        .mount => |m| if (std.fs.path.isAbsolute(m.mountpoint)) m.mountpoint else null,
        else => null,
    };
}

/// Is `cmdline` the orderly stop's verb for `mountpoint` (`squeezefs
/// umount [--lazy] <mountpoint>`)?
pub fn cmdlineUmounts(cmdline: []const u8, mountpoint: []const u8) bool {
    const verb = verbOfCmdline(cmdline) orelse return false;
    return switch (verb) {
        .umount => |u| std.mem.eql(u8, u.mountpoint, mountpoint),
        else => false,
    };
}

fn verbOfCmdline(cmdline: []const u8) ?args.Verb {
    var argv: [32][]const u8 = undefined;
    var n: usize = 0;
    var words = std.mem.splitScalar(u8, cmdline, 0);
    while (words.next()) |w| {
        if (w.len == 0) continue;
        if (n == argv.len) return null;
        argv[n] = w;
        n += 1;
    }
    var diag: args.Refusal = undefined;
    return args.parse(argv[0..n], &diag) catch null;
}

/// Two PATH_MAX paths, the options and the flags fit; a longer argv is
/// another program's.
const CMDLINE_CAP: usize = 16 << 10;

/// The mountpoint `pid`'s argv names (a `squeezefs mount` line), copied
/// into `out`; null when it runs another verb or is unreadable.
fn mountpointOfPid(pid: linux.pid_t, out: *[linux.PATH_MAX]u8) ?[]const u8 {
    var pb: [64]u8 = undefined;
    const path = std.fmt.bufPrintSentinel(&pb, "/proc/{d}/cmdline", .{pid}, 0) catch return null;
    var buf: [CMDLINE_CAP]u8 = undefined;
    const text = readSmall(path.ptr, &buf) orelse return null;
    const mp = mountpointOfCmdline(text) orelse return null;
    if (mp.len > out.len) return null;
    @memcpy(out[0..mp.len], mp);
    return out[0..mp.len];
}

/// Does a `squeezefs umount <mountpoint>` process stand in the process
/// table (this process aside)? One `/proc` walk, every cmdline parsed by
/// the grammar (`cli/umount.daemonByProcess`'s shape).
pub fn umountInFlight(io: Io, mountpoint: []const u8) bool {
    var cmdline_buf: [CMDLINE_CAP]u8 = undefined;
    var proc_dir = Io.Dir.cwd().openDir(io, "/proc", .{ .iterate = true }) catch return false;
    defer proc_dir.close(io);
    var it = proc_dir.iterate();
    while (it.next(io) catch null) |ent| {
        const pid = std.fmt.parseInt(linux.pid_t, ent.name, 10) catch continue; // a pid directory, decimal
        if (pid == linux.getpid()) continue;
        var pb: [64]u8 = undefined;
        const path = std.fmt.bufPrint(&pb, "/proc/{d}/cmdline", .{pid}) catch continue;
        // A proc file reports size 0: read into a buffer, never sized by stat.
        const cmdline = Io.Dir.cwd().readFile(io, path, &cmdline_buf) catch continue;
        if (cmdlineUmounts(cmdline, mountpoint)) return true;
    }
    return false;
}

/// The locker of an exclusive `flock`, off `/proc/locks`.
pub const Holder = struct {
    pid: linux.pid_t,
    /// This process, or a process running this very executable. A locker
    /// whose exe is unreadable (another user's, a pid outside our
    /// namespace) is not — the bound refuses it.
    squeezefs: bool,
    /// A squeezefs locker's last judgement (`judgeExiting`): null = live.
    exiting: ?Exiting = null,

    /// `pid 1234 (systemd-udevd)`: the comm read now (a refusal's text).
    pub fn format(self: Holder, w: *Io.Writer) Io.Writer.Error!void {
        var pb: [32]u8 = undefined;
        var cb: [64]u8 = undefined;
        const path = std.fmt.bufPrintSentinel(&pb, "/proc/{d}/comm", .{self.pid}, 0) catch return w.print("pid {d}", .{self.pid});
        const comm = if (readSmall(path.ptr, &cb)) |t| std.mem.trimEnd(u8, t, "\n") else "exited";
        try w.print("pid {d} ({s})", .{ self.pid, comm });
    }
};

/// The subject of a refusal `flockExclusive` answered `.AGAIN`, naming the
/// locker it last saw and, for an exiting one, the ceiling it outlasted.
pub fn writeLocker(w: *Io.Writer, locker: ?Holder) Io.Writer.Error!void {
    const h = locker orelse return w.writeAll("another process holds the writer lock");
    if (h.exiting) |why| return w.print("{f} held the writer lock past the {d} s orderly-exit ceiling while exiting ({s})", .{ h, EXITING_HOLDER_WAIT_NS / std.time.ns_per_s, why.text() });
    if (h.squeezefs) return w.print("another squeezefs process, {f}, holds the writer lock", .{h});
    try w.print("{f}, not a squeezefs process, held the writer lock past the {d} s wait", .{ h, SHARED_LOCK_WAIT_NS / std.time.ns_per_s });
}

/// The ONE `/proc/locks` read behind every writer-lock judgement: the
/// exclusive `flock` locker of `st`'s inode (`.INO | .MNT_ID` statx), null =
/// none. The device is the file's MOUNT's, off mountinfo: `/proc/locks` and
/// mountinfo both print the superblock's `s_dev`, while statx reports btrfs's
/// per-subvolume anonymous device (a guard blind to every lock on btrfs).
fn lockerOf(st: *const linux.Statx) error{Unreadable}!?Holder {
    var buf: [PROC_LOCKS_CAP]u8 = undefined;
    if (!st.mask.MNT_ID) return error.Unreadable;
    const mount = mountinfo.entryOf(readSmall(mountinfo.path, &buf) orelse return error.Unreadable, st.mnt_id) orelse return error.Unreadable;
    // Copied out: `buf` is reused for `/proc/locks` below.
    const dev_major, const dev_minor = .{ mount.dev_major, mount.dev_minor };
    const text = readSmall("/proc/locks", &buf) orelse return error.Unreadable;
    const pid = flockHolderIn(text, dev_major, dev_minor, st.ino) orelse return null;
    return .{ .pid = pid, .squeezefs = pid == linux.getpid() or runsOurExe(pid) };
}

fn runsOurExe(pid: linux.pid_t) bool {
    var pb: [32]u8 = undefined;
    const exe = std.fmt.bufPrintSentinel(&pb, "/proc/{d}/exe", .{pid}, 0) catch return false;
    var theirs: linux.Statx = undefined;
    var ours: linux.Statx = undefined;
    if (linux.errno(linux.statx(linux.AT.FDCWD, exe, 0, .{ .INO = true }, &theirs)) != .SUCCESS) return false;
    if (linux.errno(linux.statx(linux.AT.FDCWD, "/proc/self/exe", 0, .{ .INO = true }, &ours)) != .SUCCESS) return false;
    return theirs.ino == ours.ino and theirs.dev_major == ours.dev_major and theirs.dev_minor == ours.dev_minor;
}

/// The `/proc/locks` scan: the locker pid of an exclusive FLOCK line whose
/// `MAJ:MIN:INO` (hex major and minor, decimal inode) names the file (0 when
/// its pid field does not parse).
pub fn flockHolderIn(text: []const u8, dev_major: u32, dev_minor: u32, ino: u64) ?linux.pid_t {
    var lines = std.mem.splitScalar(u8, text, '\n');
    while (lines.next()) |line| {
        var it = std.mem.tokenizeAny(u8, line, " \t");
        _ = it.next() orelse continue;
        const kind = it.next() orelse continue;
        if (!std.mem.eql(u8, kind, "FLOCK")) continue;
        _ = it.next() orelse continue;
        const mode = it.next() orelse continue;
        if (!std.mem.eql(u8, mode, "WRITE")) continue;
        const pid = it.next() orelse continue;
        const where = it.next() orelse continue;
        var parts = std.mem.splitScalar(u8, where, ':');
        const maj = std.fmt.parseInt(u32, parts.next() orelse continue, 16) catch continue;
        const min = std.fmt.parseInt(u32, parts.next() orelse continue, 16) catch continue;
        const node = std.fmt.parseInt(u64, parts.next() orelse continue, 10) catch continue;
        if (maj != dev_major or min != dev_minor or node != ino) continue;
        return std.fmt.parseInt(linux.pid_t, pid, 10) catch 0; // procfs prints the pid in decimal
    }
    return null;
}

/// `/etc/nvme/hostnqn` + `/etc/nvme/hostid`, trimmed; absent reads empty.
/// Never written (quirk Q7).
pub const EtcNvme = struct {
    nqn_buf: [256]u8 = undefined,
    id_buf: [64]u8 = undefined,
    nqn_len: usize = 0,
    id_len: usize = 0,

    pub fn read() EtcNvme {
        var self: EtcNvme = .{};
        if (readSmall("/etc/nvme/hostnqn", &self.nqn_buf)) |t| {
            const trimmed = std.mem.trim(u8, t, " \t\r\n");
            self.nqn_len = trimmed.len;
            std.mem.copyForwards(u8, self.nqn_buf[0..trimmed.len], trimmed);
        }
        if (readSmall("/etc/nvme/hostid", &self.id_buf)) |t| {
            const trimmed = std.mem.trim(u8, t, " \t\r\n");
            self.id_len = trimmed.len;
            std.mem.copyForwards(u8, self.id_buf[0..trimmed.len], trimmed);
        }
        return self;
    }

    pub fn hostnqn(self: *const EtcNvme) []const u8 {
        return self.nqn_buf[0..self.nqn_len];
    }

    pub fn hostid(self: *const EtcNvme) []const u8 {
        return self.id_buf[0..self.id_len];
    }
};

const testing = std.testing;

test "the boot id parses as a uuid (dashed or bare); garbage and wrong lengths read as zero" {
    const u = parseUuid("65e6547a-03d6-4ccf-8629-db66b899865d").?;
    try testing.expectEqual(@as(u8, 0x65), u[0]);
    try testing.expectEqual(@as(u8, 0x5d), u[15]);
    try testing.expectEqual(u, parseUuid("65e6547a03d64ccf8629db66b899865d").?);
    try testing.expect(parseUuid("65e6547a-03d6") == null);
    try testing.expect(parseUuid("zz") == null);
    try testing.expect(parseUuid("65e6547a-03d6-4ccf-8629-db66b899865d00") == null);
    const live = bootId();
    try testing.expect(!std.mem.allEqual(u8, &live, 0));
}

test "start ticks: field 22 after the last ')' (a comm with spaces and parentheses does not desync); our own pid reads a value; a reaped child reads null" {
    try testing.expectEqual(@as(?u64, 6559818), parseStartTicks("1234 (a b) (c) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 6559818 20 21"));
    try testing.expect(parseStartTicks("garbage") == null);
    try testing.expect(parseStartTicks("1 (x) S 1 2") == null);
    const me = localIdentity();
    try testing.expect(me.start_ticks > 0);
    try testing.expectEqual(@as(?u64, me.start_ticks), startTicksOf(me.pid));
    try testing.expect(!probe.provablyDead(me.pid, me.start_ticks));
    // Another incarnation of our pid (different start ticks) is dead by the law.
    try testing.expect(probe.provablyDead(me.pid, me.start_ticks + 1));
    // A child that exited and was reaped: /proc/<pid> gone.
    var child = try std.process.spawn(testing.io, .{ .argv = &.{"true"}, .stdin = .ignore, .stdout = .ignore, .stderr = .ignore });
    const pid: u32 = @intCast(child.id.?);
    _ = try child.wait(testing.io);
    try testing.expect(startTicksOf(pid) == null);
    try testing.expect(probe.provablyDead(pid, 1));
}

test "a ZOMBIE holder is dead: a daemon that exited under a supervisor that reaps on its 5 s probe still has /proc/<pid>/stat (state Z, its start ticks) — it holds no fd, writes nothing, and its pid cannot be reused, so the claim it left is reclaimable at once (xfstests' _flakey_drop_and_remount remounts within 50 ms)" {
    try testing.expect(parseStartTicks("1234 (sqz) Z 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 6559818 20 21") == null);
    try testing.expect(parseStartTicks("1234 (sqz) X 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 6559818 20 21") == null);
    // A real zombie: a child that exited and is not yet reaped.
    var child = try std.process.spawn(testing.io, .{ .argv = &.{"true"}, .stdin = .ignore, .stdout = .ignore, .stderr = .ignore });
    const pid: u32 = @intCast(child.id.?);
    defer _ = child.wait(testing.io) catch {};
    var pb: [64]u8 = undefined;
    const path = try std.fmt.bufPrintSentinel(&pb, "/proc/{d}/stat", .{pid}, 0);
    var buf: [1024]u8 = undefined;
    // Wait for the exit (the stat's state letter turns Z); no reap yet.
    var polls: u32 = 0;
    while (polls < 2000) : (polls += 1) {
        const text = readSmall(path.ptr, &buf) orelse break;
        const close = std.mem.lastIndexOfScalar(u8, text, ')') orelse break;
        if (close + 2 < text.len and text[close + 2] == 'Z') break;
        try testing.io.sleep(.fromMilliseconds(1), .awake);
    }
    try testing.expect(startTicksOf(pid) == null);
    try testing.expect(probe.provablyDead(pid, 1));
}

test "/proc/locks: an exclusive FLOCK line on the file's (major, minor, inode) is the writer's guard; a shared flock, a POSIX lock or another inode is not; our own exclusive flock on a scratch file is seen without taking a second lock" {
    const table =
        \\1: FLOCK  ADVISORY  WRITE 4242 fd:01:131074 0 EOF
        \\2: FLOCK  ADVISORY  READ  4243 fd:01:131075 0 EOF
        \\3: POSIX  ADVISORY  WRITE 4244 fd:01:131076 0 EOF
        \\4: FLOCK  ADVISORY  WRITE 4245 00:1f:9 0 EOF
        \\
    ;
    try testing.expectEqual(@as(?linux.pid_t, 4242), flockHolderIn(table, 0xfd, 1, 131074));
    try testing.expectEqual(null, flockHolderIn(table, 0xfd, 1, 131075));
    try testing.expectEqual(null, flockHolderIn(table, 0xfd, 1, 131076));
    try testing.expectEqual(@as(?linux.pid_t, 4245), flockHolderIn(table, 0, 0x1f, 9));
    try testing.expectEqual(null, flockHolderIn(table, 0xfd, 2, 131074));
    try testing.expectEqual(null, flockHolderIn("garbage\n\n: : :\n", 1, 1, 1));
    // A real file: no lock, then our exclusive flock, then released. Pid-
    // tagged: two test processes on the venue never share the probe.
    var path_buf: [64]u8 = undefined;
    const path = try std.fmt.bufPrintSentinel(&path_buf, "/build/tmp/proc-flock-probe-{d}", .{linux.getpid()}, 0);
    _ = linux.mkdir("/build/tmp", 0o755);
    const rc = linux.open(path.ptr, .{ .ACCMODE = .RDWR, .CLOEXEC = true, .CREAT = true }, 0o600);
    if (linux.errno(rc) != .SUCCESS) return testkit.skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    const fd: linux.fd_t = @intCast(rc);
    defer {
        _ = linux.close(fd);
        _ = linux.unlink(path.ptr);
    }
    try testing.expect(!writerFlockHeld(testing.io, path, null));
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.flock(fd, std.posix.LOCK.EX | std.posix.LOCK.NB)));
    try testing.expect(writerFlockHeld(testing.io, path, null));
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.flock(fd, std.posix.LOCK.UN)));
    try testing.expect(!writerFlockHeld(testing.io, path, null));
}

test "flockExclusive waits out an EXCLUSIVE holder that is not this binary — systemd-udevd's flock(LOCK_EX) + BLKRRPART after every write-close of a whole disk — and opens once it lets go; an exclusive holder of this binary (here: this process) still refuses at once" {
    var path_buf: [64]u8 = undefined;
    const path = try std.fmt.bufPrintSentinel(&path_buf, "/build/tmp/proc-flock-transient-{d}", .{linux.getpid()}, 0);
    _ = linux.mkdir("/build/tmp", 0o755);
    const rc = linux.open(path.ptr, .{ .ACCMODE = .RDWR, .CLOEXEC = true, .CREAT = true }, 0o600);
    if (linux.errno(rc) != .SUCCESS) return testkit.skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    const fd: linux.fd_t = @intCast(rc);
    defer {
        _ = linux.close(fd);
        _ = linux.unlink(path.ptr);
    }
    var waits: counters.ShardedCounter = .{};
    // This process holds it from another open file: refused, no wait.
    {
        const rc2 = linux.open(path.ptr, .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc2));
        const own: linux.fd_t = @intCast(rc2);
        defer _ = linux.close(own);
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.flock(own, std.posix.LOCK.EX | std.posix.LOCK.NB)));
        var locker: ?Holder = null;
        var exiting_waits: counters.ShardedCounter = .{};
        try testing.expectEqual(linux.E.AGAIN, flockExclusive(testing.io, fd, .{ .shared = &waits, .exiting = &exiting_waits }, &locker));
        try testing.expectEqual(@as(u64, 0), waits.load());
        try testing.expectEqual(@as(u64, 0), exiting_waits.load());
        // This process runs no mount verb: a live holder, judged so.
        try testing.expect(locker != null and locker.?.squeezefs and locker.?.pid == linux.getpid() and locker.?.exiting == null);
    }
    // The stand-in for the udev worker: flock(1) holding LOCK_EX until its
    // `cat` reads EOF on our pipe.
    var holder = try std.process.spawn(testing.io, .{ .argv = &.{ "flock", "-x", path, "cat" }, .stdin = .pipe, .stdout = .ignore, .stderr = .ignore });
    defer if (holder.stdin) |s| {
        s.close(testing.io);
        holder.stdin = null;
        _ = holder.wait(testing.io) catch {};
    };
    try awaitLocker(path, @intCast(holder.id.?));
    const Opener = struct {
        fn run(f: linux.fd_t, w: *counters.ShardedCounter, out: *linux.E, done: *std.atomic.Value(bool)) void {
            var locker: ?Holder = null;
            out.* = flockExclusive(testing.io, f, .{ .shared = w }, &locker);
            done.store(true, .release);
        }
    };
    var result: linux.E = .SUCCESS;
    var done: std.atomic.Value(bool) = .init(false);
    const t = try std.Thread.spawn(.{}, Opener.run, .{ fd, &waits, &result, &done });
    // The opener met the holder and is waiting (or, refusing it, is done):
    // only then let go.
    while (waits.load() == 0 and !done.load(.acquire)) try std.Thread.yield();
    holder.stdin.?.close(testing.io);
    holder.stdin = null;
    _ = try holder.wait(testing.io);
    t.join();
    try testing.expectEqual(linux.E.SUCCESS, result);
    try testing.expect(waits.load() > 0);
}

/// A scratch file pid-tagged under `/build/tmp`; null = not writable here.
fn scratch(buf: []u8, tag: []const u8) !?[:0]const u8 {
    const path = try std.fmt.bufPrintSentinel(buf, "/build/tmp/proc-flock-{s}-{d}", .{ tag, linux.getpid() }, 0);
    _ = linux.mkdir("/build/tmp", 0o755);
    const rc = linux.open(path.ptr, .{ .ACCMODE = .RDWR, .CLOEXEC = true, .CREAT = true }, 0o600);
    if (linux.errno(rc) != .SUCCESS) return null;
    _ = linux.close(@intCast(rc));
    return path;
}

/// flock(1) (`pid`) took its lock on `path`: its line is in `/proc/locks`.
fn awaitLocker(path: []const u8, pid: linux.pid_t) !void {
    const hang_guard = clock.monotonicNs() + 60 * std.time.ns_per_s;
    while (exclusiveLocker(path) == null) {
        if (clock.monotonicNs() >= hang_guard) {
            std.debug.print("\nflock(1) pid {d} never took the lock on {s}\n", .{ pid, path });
            return error.HolderNeverLocked;
        }
        try std.Thread.yield();
    }
}

test "writerFlockHeld (the readers' 'is a writer here?') waits out an exclusive holder that is not this binary — udevd's re-read after the writer's last close — and reads 'no writer' once it lets go; this process's own holder reads 'writer' with no wait" {
    var path_buf: [64]u8 = undefined;
    const path = (try scratch(&path_buf, "reader")) orelse return testkit.skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    defer _ = linux.unlink(path.ptr);
    var waits: counters.ShardedCounter = .{};
    {
        const rc = linux.open(path.ptr, .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc));
        const own: linux.fd_t = @intCast(rc);
        defer _ = linux.close(own);
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.flock(own, std.posix.LOCK.EX | std.posix.LOCK.NB)));
        try testing.expect(writerFlockHeld(testing.io, path, &waits));
        try testing.expectEqual(@as(u64, 0), waits.load());
    }
    var holder = try std.process.spawn(testing.io, .{ .argv = &.{ "flock", "-x", path, "cat" }, .stdin = .pipe, .stdout = .ignore, .stderr = .ignore });
    defer if (holder.stdin) |s| {
        s.close(testing.io);
        holder.stdin = null;
        _ = holder.wait(testing.io) catch {};
    };
    try awaitLocker(path, @intCast(holder.id.?));
    const Reader = struct {
        fn run(p: []const u8, w: *counters.ShardedCounter, out: *bool, done: *std.atomic.Value(bool)) void {
            out.* = writerFlockHeld(testing.io, p, w);
            done.store(true, .release);
        }
    };
    var held = true;
    var done: std.atomic.Value(bool) = .init(false);
    const t = try std.Thread.spawn(.{}, Reader.run, .{ path, &waits, &held, &done });
    // The reader met the holder and is waiting (or, judging it, is done):
    // only then let go.
    while (waits.load() == 0 and !done.load(.acquire)) try std.Thread.yield();
    holder.stdin.?.close(testing.io);
    holder.stdin = null;
    _ = try holder.wait(testing.io);
    t.join();
    try testing.expect(!held);
    try testing.expect(waits.load() > 0);
}

test "an exclusive holder that is not this binary and outlasts the bound: flockExclusive refuses and the refusal names it by pid and comm ('pid N (flock)', not 'another squeezefs process'); writerFlockHeld reads it as a writer" {
    var path_buf: [64]u8 = undefined;
    const path = (try scratch(&path_buf, "long")) orelse return testkit.skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    defer _ = linux.unlink(path.ptr);
    const rc = linux.open(path.ptr, .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc));
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    var holder = try std.process.spawn(testing.io, .{ .argv = &.{ "flock", "-x", path, "cat" }, .stdin = .pipe, .stdout = .ignore, .stderr = .ignore });
    defer {
        holder.stdin.?.close(testing.io);
        holder.stdin = null;
        _ = holder.wait(testing.io) catch {};
    }
    try awaitLocker(path, @intCast(holder.id.?));
    // Both judgements wait out the same bound: run them side by side.
    const Reader = struct {
        fn run(p: []const u8, out: *bool) void {
            out.* = writerFlockHeld(testing.io, p, null);
        }
    };
    var held = false;
    const t = try std.Thread.spawn(.{}, Reader.run, .{ path, &held });
    var locker: ?Holder = null;
    const e = flockExclusive(testing.io, fd, .{}, &locker);
    t.join();
    try testing.expectEqual(linux.E.AGAIN, e);
    try testing.expect(held);
    try testing.expect(locker != null);
    try testing.expectEqual(@as(linux.pid_t, @intCast(holder.id.?)), locker.?.pid);
    try testing.expect(!locker.?.squeezefs);
    var buf: [192]u8 = undefined;
    var w: Io.Writer = .fixed(&buf);
    try writeLocker(&w, locker);
    var want_buf: [32]u8 = undefined;
    const want = try std.fmt.bufPrint(&want_buf, "pid {d} (flock)", .{locker.?.pid});
    if (std.mem.indexOf(u8, w.buffered(), want) == null or std.mem.indexOf(u8, w.buffered(), "another squeezefs") != null) {
        std.debug.print("\nthe refusal: {s}\n", .{w.buffered()});
        return error.LockerUnnamed;
    }
}

test "the orderly-exit ceiling is the umount verb's exit bound plus two ticks (the pre-signal census, the replay) and dwarfs udev's wait; the exiting step is the verb's exit-poll quantum; a live foreign locker is never judged exiting" {
    try testing.expectEqual(umount_verb.DAEMON_EXIT_WAIT_NS + 2 * tick_law.tick_ns, EXITING_HOLDER_WAIT_NS);
    try testing.expect(EXITING_HOLDER_WAIT_NS > SHARED_LOCK_WAIT_NS);
    try testing.expectEqual(@as(i64, 20), EXITING_RETRY_MS);
    try testing.expectEqual(@as(?Exiting, null), judgeExiting(testing.io, linux.getpid(), false));
    // This process (alive, no mount verb) is a live holder of this binary.
    try testing.expectEqual(@as(?Exiting, null), judgeExiting(testing.io, linux.getpid(), true));
}

test "a cmdline is a mount verb's iff the CLI grammar reads it so, and its mountpoint must be absolute; the umount verb's cmdline names its mountpoint through the same grammar (flags in any order); other verbs, garbage and an empty line name nothing" {
    const daemon_line = "squeezefs\x00mount\x00--daemon\x00--supervise\x00--log-file\x00/var/log/sqz.log\x00-o\x00default_permissions,allow_other\x00sqmeta:///dev/mapper/meta\x00/mnt/scratch\x00";
    try testing.expectEqualStrings("/mnt/scratch", mountpointOfCmdline(daemon_line).?);
    try testing.expectEqualStrings("/mnt/a", mountpointOfCmdline("squeezefs\x00mount\x00sqmeta:///x\x00/mnt/a\x00").?);
    try testing.expect(mountpointOfCmdline("squeezefs\x00mount\x00sqmeta:///x\x00mnt\x00") == null);
    try testing.expect(mountpointOfCmdline("squeezefs\x00umount\x00/mnt/a\x00") == null);
    try testing.expect(mountpointOfCmdline("squeezefs\x00fsck\x00--offline\x00sqmeta:///x\x00") == null);
    try testing.expect(mountpointOfCmdline("bash\x00-c\x00mount\x00") == null);
    try testing.expect(mountpointOfCmdline("") == null);
    try testing.expect(mountpointOfCmdline("squeezefs\x00") == null);
    try testing.expect(cmdlineUmounts("squeezefs\x00umount\x00/mnt/a\x00", "/mnt/a"));
    try testing.expect(cmdlineUmounts("squeezefs\x00umount\x00--lazy\x00/mnt/a\x00", "/mnt/a"));
    try testing.expect(cmdlineUmounts("squeezefs\x00umount\x00/mnt/a\x00-l\x00", "/mnt/a"));
    try testing.expect(!cmdlineUmounts("squeezefs\x00umount\x00/mnt/a\x00", "/mnt/ab"));
    try testing.expect(!cmdlineUmounts("squeezefs\x00umount\x00/mnt/a\x00", "/mnt"));
    try testing.expect(!cmdlineUmounts("squeezefs\x00mount\x00sqmeta:///x\x00/mnt/a\x00", "/mnt/a"));
    try testing.expect(!cmdlineUmounts("umount\x00/mnt/a\x00", "/mnt/a"));
    try testing.expect(!cmdlineUmounts("", "/mnt/a"));
}

test "umountInFlight: a process running `squeezefs umount <dir>` (a shell stand-in under that argv) is seen for exactly that directory and for no other; gone once it exits" {
    _ = linux.mkdir("/build/tmp", 0o755);
    var db: [64:0]u8 = undefined;
    const dir = try std.fmt.bufPrintSentinel(&db, "/build/tmp/proc-umount-verb-{d}", .{linux.getpid()}, 0);
    if (linux.errno(linux.mkdir(dir.ptr, 0o755)) != .SUCCESS) return testkit.skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    defer Io.Dir.cwd().deleteTree(testing.io, dir) catch {};
    // The stand-in: `/bin/sh` exec'd under the verb's argv (`squeezefs
    // umount <dir>`), running the command file `umount` in `dir` (a
    // shell's command file is its first word), which says ready on stdout
    // and parks on stdin until EOF.
    var sp: [96]u8 = undefined;
    try Io.Dir.cwd().writeFile(testing.io, .{ .sub_path = try std.fmt.bufPrint(&sp, "{s}/umount", .{dir}), .data = "echo ok\nread x\n" });
    var park: [2]linux.fd_t = undefined;
    var ready: [2]linux.fd_t = undefined;
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.pipe2(&park, .{ .CLOEXEC = true })));
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.pipe2(&ready, .{ .CLOEXEC = true })));
    const rc = linux.fork();
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc));
    if (rc == 0) {
        _ = linux.chdir(dir.ptr);
        _ = linux.dup2(park[0], 0);
        _ = linux.dup2(ready[1], 1);
        const argv = [_:null]?[*:0]const u8{ "squeezefs", "umount", dir.ptr };
        const envp = [_:null]?[*:0]const u8{};
        _ = linux.execve("/bin/sh", &argv, &envp);
        linux.exit(127);
    }
    const verb: linux.pid_t = @intCast(rc);
    _ = linux.close(park[0]);
    _ = linux.close(ready[1]);
    var reaped = false;
    defer if (!reaped) {
        _ = linux.close(park[1]);
        var status: i32 = 0;
        _ = linux.waitpid(verb, &status, 0);
    };
    var byte: [3]u8 = undefined;
    _ = linux.read(ready[0], &byte, 3);
    _ = linux.close(ready[0]);
    try testing.expect(umountInFlight(testing.io, dir));
    try testing.expect(!umountInFlight(testing.io, "/build/tmp/proc-umount-verb-other"));
    try testing.expect(!umountInFlight(testing.io, "/build/tmp"));
    // EOF on its stdin ends the verb.
    _ = linux.close(park[1]);
    var status: i32 = 0;
    _ = linux.waitpid(verb, &status, 0);
    reaped = true;
    try testing.expect(!umountInFlight(testing.io, dir));
}

test "the replay hold: a writer lock whose CREATOR has exited while its open file description lives on (the supervisor's hold through the journal tail's replay) is judged `replay` and waited out — flockExclusive counts the exiting waits, names the holder, and succeeds once the description closes; the shared-holder counter stays 0" {
    var path_buf: [64]u8 = undefined;
    const path = (try scratch(&path_buf, "replay")) orelse return testkit.skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    defer _ = linux.unlink(path.ptr);
    // The description the lock will live on: opened here, so it outlives
    // the child that takes the lock (the daemon) — this process is the
    // supervisor's part.
    const rc_s = linux.open(path.ptr, .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc_s));
    const supervisor_fd: linux.fd_t = @intCast(rc_s);
    var released = false;
    defer if (!released) {
        _ = linux.close(supervisor_fd);
    };
    const rc = linux.fork();
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc));
    if (rc == 0) {
        _ = linux.flock(supervisor_fd, std.posix.LOCK.EX | std.posix.LOCK.NB);
        linux.exit(0);
    }
    const creator: linux.pid_t = @intCast(rc);
    var status: i32 = 0;
    _ = linux.waitpid(creator, &status, 0);
    // The lock stands with a dead creator: `/proc/locks` names its pid,
    // whose exe is unreadable now (not "squeezefs") — the replay all the same.
    const seen = exclusiveLocker(path).?;
    try testing.expectEqual(creator, seen.pid);
    try testing.expectEqual(@as(?Exiting, .replay), judgeExiting(testing.io, creator, seen.squeezefs));
    const rc2 = linux.open(path.ptr, .{ .ACCMODE = .RDWR, .CLOEXEC = true }, 0);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc2));
    const fd: linux.fd_t = @intCast(rc2);
    defer _ = linux.close(fd);
    var shared: counters.ShardedCounter = .{};
    var exiting: counters.ShardedCounter = .{};
    const Opener = struct {
        fn run(f: linux.fd_t, w: Waits, out: *linux.E, holder: *?Holder, done: *std.atomic.Value(bool)) void {
            out.* = flockExclusive(testing.io, f, w, holder);
            done.store(true, .release);
        }
    };
    var result: linux.E = .SUCCESS;
    var locker: ?Holder = null;
    var done: std.atomic.Value(bool) = .init(false);
    const t = try std.Thread.spawn(.{}, Opener.run, .{ fd, Waits{ .shared = &shared, .exiting = &exiting }, &result, &locker, &done });
    // The opener met the dead creator's lock and waits: only then release.
    while (exiting.load() == 0 and !done.load(.acquire)) try std.Thread.yield();
    _ = linux.close(supervisor_fd);
    released = true;
    t.join();
    try testing.expectEqual(linux.E.SUCCESS, result);
    try testing.expect(exiting.load() > 0);
    try testing.expectEqual(@as(u64, 0), shared.load());
    try testing.expect(locker != null and locker.?.pid == creator and locker.?.exiting == .replay);
    // The refusal text an outlasted ceiling would carry names the class.
    var buf: [256]u8 = undefined;
    var w: Io.Writer = .fixed(&buf);
    try writeLocker(&w, locker);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "orderly-exit ceiling while exiting (its process is gone") != null);
}

/// `argv` ran and exited 0 (its output dropped).
fn runOk(argv: []const []const u8) bool {
    var child = std.process.spawn(testing.io, .{ .argv = argv, .stdin = .ignore, .stdout = .ignore, .stderr = .ignore }) catch return false;
    const term = child.wait(testing.io) catch return false;
    return term == .exited and term.exited == 0;
}

test "a file on btrfs, whose statx device is the subvolume's anonymous one while /proc/locks prints the superblock's: exclusiveLocker still finds flock(1)'s exclusive lock (a writer guard blind on Omarchy's default filesystem)" {
    if (linux.getuid() != 0) return testkit.skip(@src(), .no_root, "a loop mount needs root");
    var dir_buf: [64]u8 = undefined;
    const dir = try std.fmt.bufPrintSentinel(&dir_buf, "/build/tmp/proc-btrfs-{d}", .{linux.getpid()}, 0);
    var img_buf: [72]u8 = undefined;
    const img = try std.fmt.bufPrintSentinel(&img_buf, "{s}.img", .{dir}, 0);
    var file_buf: [72]u8 = undefined;
    const file = try std.fmt.bufPrintSentinel(&file_buf, "{s}/f", .{dir}, 0);
    defer _ = linux.unlink(img.ptr);
    if (!runOk(&.{ "truncate", "-s", "256M", img }) or !runOk(&.{ "mkfs.btrfs", "-q", "-f", img }))
        return testkit.skip(@src(), .no_device, "no mkfs.btrfs here (btrfs-progs)");
    _ = linux.mkdir(dir.ptr, 0o700);
    defer _ = linux.rmdir(dir.ptr);
    if (!runOk(&.{ "mount", "-o", "loop", img, dir })) return testkit.skip(@src(), .no_device, "no loop-mounted btrfs here (the loop driver, the btrfs module)");
    defer _ = runOk(&.{ "umount", dir });
    const rc = linux.open(file.ptr, .{ .ACCMODE = .RDWR, .CLOEXEC = true, .CREAT = true }, 0o600);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(rc));
    _ = linux.close(@intCast(rc));
    var holder = try std.process.spawn(testing.io, .{ .argv = &.{ "flock", "-x", file, "cat" }, .stdin = .pipe, .stdout = .ignore, .stderr = .ignore });
    defer {
        holder.stdin.?.close(testing.io);
        holder.stdin = null;
        _ = holder.wait(testing.io) catch {};
    }
    try awaitLocker(file, @intCast(holder.id.?));
}

test "/etc/nvme reads as empty when absent, never written" {
    const e = EtcNvme.read();
    _ = e.hostnqn();
    _ = e.hostid();
    var st: linux.Statx = undefined;
    const before = linux.errno(linux.statx(linux.AT.FDCWD, "/etc/nvme/hostid", 0, .{ .TYPE = true }, &st));
    _ = EtcNvme.read();
    try testing.expectEqual(before, linux.errno(linux.statx(linux.AT.FDCWD, "/etc/nvme/hostid", 0, .{ .TYPE = true }, &st)));
}

fn procTextFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [512]u8 = undefined;
    const len = smith.slice(&buf);
    const text = buf[0..len];
    _ = parseUuid(text);
    _ = parseStartTicks(text);
}

test "fuzz: the boot-id uuid and the /proc/<pid>/stat start-ticks parsers are total on arbitrary text" {
    try fuzzkit.fuzz({}, procTextFuzz, .{ .corpus = &.{ fuzzkit.slice("c1c1c1c1-c1c1-4c1c-8c1c-c1c1c1c1c1c1"), fuzzkit.slice("1 (a) b) S 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 42 0\n"), fuzzkit.slice(")") } });
}
