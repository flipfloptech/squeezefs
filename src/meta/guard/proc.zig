//! The guard's host readers (Linux): the local writer identity (boot id,
//! pid, `/proc/self/stat` start ticks, hostname), the dead-pid prover over
//! `/proc/<pid>/stat`, and the `/etc/nvme/{hostnqn,hostid}` defaults —
//! READ ONLY (quirk Q7: a mount never mints or writes host configuration;
//! an absent file reads as empty).

const std = @import("std");
const fuzzkit = @import("../../core/fuzzkit.zig");
const testkit = @import("../../core/testkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const claim = @import("claim.zig");

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
/// which the proof reads the same way: nothing alive answers to the pid).
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
        if (idx == 22) return std.fmt.parseInt(u64, tok, 10) catch null;
    }
    return null;
}

/// A zombie still has a `/proc/<pid>/stat` with its original start ticks;
/// its parent has not reaped it, so it IS the author — not provably dead
/// (the smoke reaps its children before remounting).
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

/// Is an exclusive `flock` (the D0 guard's layer A) held on `path` by any
/// process of this host — read off `/proc/locks`, so the probe takes NO
/// lock of its own (a `-o ro` reader "claims nothing", and a transient
/// `LOCK_SH` would refuse a writer's `LOCK_EX | LOCK_NB` racing it).
/// Unreadable procfs reads as "no lock" (the claim face judges then).
pub fn writerFlockHeld(path: []const u8) bool {
    var zbuf: [linux.PATH_MAX]u8 = undefined;
    const z = std.fmt.bufPrintSentinel(&zbuf, "{s}", .{path}, 0) catch return false;
    var st: linux.Statx = undefined;
    if (linux.errno(linux.statx(linux.AT.FDCWD, z, 0, .{ .INO = true }, &st)) != .SUCCESS) return false;
    var buf: [PROC_LOCKS_CAP]u8 = undefined;
    const text = readSmall("/proc/locks", &buf) orelse return false;
    return flockHeldIn(text, st.dev_major, st.dev_minor, st.ino);
}

/// The `/proc/locks` scan: an exclusive FLOCK line whose `MAJ:MIN:INO`
/// (hex major and minor, decimal inode) names the file.
pub fn flockHeldIn(text: []const u8, dev_major: u32, dev_minor: u32, ino: u64) bool {
    var lines = std.mem.splitScalar(u8, text, '\n');
    while (lines.next()) |line| {
        var it = std.mem.tokenizeAny(u8, line, " \t");
        _ = it.next() orelse continue;
        const kind = it.next() orelse continue;
        if (!std.mem.eql(u8, kind, "FLOCK")) continue;
        _ = it.next() orelse continue;
        const mode = it.next() orelse continue;
        if (!std.mem.eql(u8, mode, "WRITE")) continue;
        _ = it.next() orelse continue;
        const where = it.next() orelse continue;
        var parts = std.mem.splitScalar(u8, where, ':');
        const maj = std.fmt.parseInt(u32, parts.next() orelse continue, 16) catch continue;
        const min = std.fmt.parseInt(u32, parts.next() orelse continue, 16) catch continue;
        const node = std.fmt.parseInt(u64, parts.next() orelse continue, 10) catch continue;
        if (maj == dev_major and min == dev_minor and node == ino) return true;
    }
    return false;
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

test "/proc/locks: an exclusive FLOCK line on the file's (major, minor, inode) is the writer's guard; a shared flock, a POSIX lock or another inode is not; our own exclusive flock on a scratch file is seen without taking a second lock" {
    const table =
        \\1: FLOCK  ADVISORY  WRITE 4242 fd:01:131074 0 EOF
        \\2: FLOCK  ADVISORY  READ  4243 fd:01:131075 0 EOF
        \\3: POSIX  ADVISORY  WRITE 4244 fd:01:131076 0 EOF
        \\4: FLOCK  ADVISORY  WRITE 4245 00:1f:9 0 EOF
        \\
    ;
    try testing.expect(flockHeldIn(table, 0xfd, 1, 131074));
    try testing.expect(!flockHeldIn(table, 0xfd, 1, 131075));
    try testing.expect(!flockHeldIn(table, 0xfd, 1, 131076));
    try testing.expect(flockHeldIn(table, 0, 0x1f, 9));
    try testing.expect(!flockHeldIn(table, 0xfd, 2, 131074));
    try testing.expect(!flockHeldIn("garbage\n\n: : :\n", 1, 1, 1));
    // A real file: no lock, then our exclusive flock, then released.
    const path = "/build/tmp/proc-flock-probe";
    _ = linux.mkdir("/build/tmp", 0o755);
    const rc = linux.open(path, .{ .ACCMODE = .RDWR, .CLOEXEC = true, .CREAT = true }, 0o600);
    if (linux.errno(rc) != .SUCCESS) return testkit.skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    const fd: linux.fd_t = @intCast(rc);
    defer {
        _ = linux.close(fd);
        _ = linux.unlink(path);
    }
    try testing.expect(!writerFlockHeld(path));
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.flock(fd, std.posix.LOCK.EX | std.posix.LOCK.NB)));
    try testing.expect(writerFlockHeld(path));
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.flock(fd, std.posix.LOCK.UN)));
    try testing.expect(!writerFlockHeld(path));
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
