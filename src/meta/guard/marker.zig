//! A metadata rule's marker (decision 0004 §4.4.2, revisions 22–27).
//!
//! Under the registration lock, after the re-read and before `register`,
//! the rule creates an empty `.reg-<id>`. A clean re-judge removes it
//! before the lock is released. A refusal after the register renames it to
//! `.wd-<id>` with its start time and checksum written first, so the name
//! appears only with the record complete. The unregister that follows, and
//! the removal of that `.wd` after it succeeds, are the one transition
//! outside the lock. A withdrawal that will not finish is renamed
//! `.abandoned-<id>` under the lock.
//!
//! A section marker is judged by its name: boot id, pid, start ticks.
//! Data rules make none. Linux only.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;

const checksum = @import("../../core/checksum.zig");
const clock = @import("../../core/clock.zig");
const counters = @import("../../core/counters.zig");
const log = @import("../../core/log.zig");
const claim = @import("claim.zig");
const proc = @import("proc.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("guard/marker is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

/// DC22's withdraw wait: the kill's exit bound. The tie test restates it
/// against `host_fence.KILL_WAIT_MS`.
pub const T_WITHDRAW_S: u64 = 10;

/// `nvme_core.io_timeout`'s default. The withdraw span assumes the live
/// value is not above this; above it, a slow unregister is the named race.
pub const NVME_IO_TIMEOUT_S: u64 = 30;

/// `T_WITHDRAW_S + NVME_IO_TIMEOUT_S + HEARTBEAT_INTERVAL_S`. A `.wd` older
/// than this is abandoned.
pub const MARKER_WITHDRAW_MAX_S: u64 = T_WITHDRAW_S + NVME_IO_TIMEOUT_S + claim.HEARTBEAT_INTERVAL_S;

pub const stats = struct {
    /// A `.wd` past the span whose rule is frozen (`T`/`t`).
    pub var host_fence_marker_wd_aged_frozen: counters.ShardedCounter = .{};
    /// A `.wd` past the span whose rule is in `D`.
    pub var host_fence_marker_wd_aged_uninterruptible: counters.ShardedCounter = .{};
    /// A `.wd` past the span whose rule is neither. The alarm: under normal
    /// load it stays 0.
    pub var host_fence_marker_wd_aged_other: counters.ShardedCounter = .{};
};

/// `EEXIST` on a section marker. The id includes a per-process sequence, so
/// this does not happen.
pub var marker_exists: counters.Tripwire = .init("host_fence_marker_exists");

/// A live `.reg` met while this thread holds the registration lock. The
/// lock has stopped excluding. Must stay 0.
pub var marker_live_reg_locked: counters.Tripwire = .init("host_fence_marker_live_reg_locked");

pub const Kind = enum { section, withdrawal, abandoned };

pub const Role = enum { mutation, lapse_acquire };

pub const Mark = struct {
    leaf: [192]u8 = undefined,
    len: usize = 0,

    fn slice(self: *const Mark) []const u8 {
        return self.leaf[0..self.len];
    }
};

const Listed = struct {
    kind: Kind,
    pid: u32,
    start_ticks: u64,
    volume: u32,
    sequence: u64,
    boot_ok: bool,
    alive: bool,
    aged: bool,
    leaf: [192]u8 = undefined,
    leaf_len: usize = 0,
};

pub const Survey = struct {
    live_section: u32 = 0,
    live_withdrawal: u32 = 0,
    dead_waiting: u32 = 0,
    dead_ready: u32 = 0,
    abandoned: u32 = 0,

    pub fn forbidsMutation(self: Survey) bool {
        return self.live_section + self.live_withdrawal + self.dead_waiting + self.dead_ready + self.abandoned != 0;
    }

    pub fn forbidsLapse(self: Survey) bool {
        return self.live_section + self.live_withdrawal + self.dead_waiting != 0;
    }

    pub fn needsCheck(self: Survey) bool {
        return self.dead_ready + self.abandoned != 0;
    }
};

var next_seq = std.atomic.Value(u64).init(1);

const Sight = struct { pid: u32, ticks: u64, seq: u64, at: u64, aged: bool = false };
var sights: [32]Sight = undefined;
var n_sights: usize = 0;
var sight_lock: std.atomic.Value(u8) = .init(0);

fn lockSights() void {
    while (sight_lock.cmpxchgWeak(0, 1, .acquire, .monotonic) != null) std.atomic.spinLoopHint();
}

fn unlockSights() void {
    sight_lock.store(0, .release);
}

fn claimAged(pid: u32, ticks: u64, seq: u64) bool {
    lockSights();
    defer unlockSights();
    for (sights[0..n_sights]) |s| if (s.pid == pid and s.ticks == ticks and s.seq == seq and s.aged) return false;
    if (n_sights == sights.len) return true;
    sights[n_sights] = .{ .pid = pid, .ticks = ticks, .seq = seq, .at = 0, .aged = true };
    n_sights += 1;
    return true;
}

fn firstSeen(pid: u32, ticks: u64, seq: u64, now: u64) u64 {
    lockSights();
    defer unlockSights();
    for (sights[0..n_sights]) |s| if (s.pid == pid and s.ticks == ticks and s.seq == seq and !s.aged) return s.at;
    if (n_sights == sights.len) return now;
    sights[n_sights] = .{ .pid = pid, .ticks = ticks, .seq = seq, .at = now };
    n_sights += 1;
    return now;
}

fn bootText(buf: *[64]u8) []const u8 {
    const text = proc.readSmall("/proc/sys/kernel/random/boot_id", buf) orelse return "";
    return std.mem.trim(u8, text, " \t\r\n");
}

fn alive(pid: u32, ticks: u64) bool {
    return switch (proc.pin(@intCast(pid), ticks)) {
        .pinned => |p| {
            var pin = p;
            pin.close();
            return true;
        },
        .gone => false,
        // Unreadable is not dead.
        .refused => true,
    };
}

fn noteAged(pid: u32) void {
    const state = threadState(pid) orelse {
        stats.host_fence_marker_wd_aged_other.inc();
        log.err("host fence: a withdrawal marker aged past {d} s and the rule's state could not be read (pid {d})", .{ MARKER_WITHDRAW_MAX_S, pid });
        return;
    };
    switch (state) {
        'T', 't' => stats.host_fence_marker_wd_aged_frozen.inc(),
        'D' => stats.host_fence_marker_wd_aged_uninterruptible.inc(),
        else => {
            stats.host_fence_marker_wd_aged_other.inc();
            log.err("host fence: a withdrawal marker aged past {d} s with the rule in state {c} (pid {d}) — host_fence_marker_wd_aged_other", .{ MARKER_WITHDRAW_MAX_S, state, pid });
        },
    }
}

fn threadState(pid: u32) ?u8 {
    var path: [64]u8 = undefined;
    const z = std.fmt.bufPrintSentinel(&path, "/proc/{d}/stat", .{pid}, 0) catch return null;
    var buf: [512]u8 = undefined;
    const text = proc.readSmall(z.ptr, &buf) orelse return null;
    const paren = std.mem.lastIndexOfScalar(u8, text, ')') orelse return null;
    if (paren + 2 >= text.len) return null;
    return text[paren + 2];
}

fn encodeWithdrawal(now_ns: u64) [16]u8 {
    var out: [16]u8 = undefined;
    std.mem.writeInt(u64, out[0..8], now_ns, .little);
    var h: checksum.Hasher = .init();
    h.update(out[0..8]);
    std.mem.writeInt(u64, out[8..16], h.finalLow64(), .little);
    return out;
}

fn withdrawalAged(bytes: []const u8, now_ns: u64) bool {
    if (bytes.len < 16) return true;
    var h: checksum.Hasher = .init();
    h.update(bytes[0..8]);
    if (h.finalLow64() != std.mem.readInt(u64, bytes[8..16], .little)) return true;
    const started = std.mem.readInt(u64, bytes[0..8], .little);
    return now_ns -| started > MARKER_WITHDRAW_MAX_S * std.time.ns_per_s;
}

const Parsed = struct {
    kind: Kind,
    boot: []const u8,
    pid: u32,
    ticks: u64,
    vol: u32,
    seq: u64,
};

fn parseLeaf(name: []const u8) ?Parsed {
    const kind: Kind = if (std.mem.startsWith(u8, name, ".abandoned-"))
        .abandoned
    else if (std.mem.startsWith(u8, name, ".reg-"))
        .section
    else if (std.mem.startsWith(u8, name, ".wd-"))
        .withdrawal
    else
        return null;
    const rest = switch (kind) {
        .abandoned => name[".abandoned-".len..],
        .section => name[".reg-".len..],
        .withdrawal => name[".wd-".len..],
    };
    var parts: [4][]const u8 = undefined;
    var cursor = rest;
    var i: usize = 4;
    while (i > 0) {
        i -= 1;
        const dash = std.mem.lastIndexOfScalar(u8, cursor, '-') orelse return null;
        parts[i] = cursor[dash + 1 ..];
        cursor = cursor[0..dash];
    }
    if (cursor.len == 0) return null;
    return .{
        .kind = kind,
        .boot = cursor,
        .pid = std.fmt.parseInt(u32, parts[0], 10) catch return null,
        .ticks = std.fmt.parseInt(u64, parts[1], 10) catch return null,
        .vol = std.fmt.parseInt(u32, parts[2], 10) catch return null,
        .seq = std.fmt.parseInt(u64, parts[3], 10) catch return null,
    };
}

fn leafPath(dir: []const u8, leaf: []const u8, buf: *[linux.PATH_MAX]u8) ?[:0]const u8 {
    return std.fmt.bufPrintSentinel(buf, "{s}/{s}", .{ dir, leaf }, 0) catch null;
}

fn createSection(dir: []const u8, volume: u32) error{ MarkerExists, Io }!Mark {
    var boot_buf: [64]u8 = undefined;
    const boot = bootText(&boot_buf);
    if (boot.len == 0) return error.Io;
    const me = proc.localIdentity();
    const seq = next_seq.fetchAdd(1, .monotonic);
    var mark: Mark = .{};
    const leaf = std.fmt.bufPrint(&mark.leaf, ".reg-{s}-{d}-{d}-{d}-{d}", .{ boot, me.pid, me.start_ticks, volume, seq }) catch return error.Io;
    mark.len = leaf.len;
    var path: [linux.PATH_MAX]u8 = undefined;
    const z = leafPath(dir, mark.slice(), &path) orelse return error.Io;
    const rc = linux.open(z.ptr, .{ .ACCMODE = .WRONLY, .CREAT = true, .EXCL = true, .CLOEXEC = true }, 0o600);
    if (linux.errno(rc) == .EXIST) {
        marker_exists.fire("a section marker already existed");
        return error.MarkerExists;
    }
    if (linux.errno(rc) != .SUCCESS) return error.Io;
    _ = linux.close(@intCast(rc));
    return mark;
}

fn unlinkLeaf(dir: []const u8, leaf: []const u8) void {
    var path: [linux.PATH_MAX]u8 = undefined;
    const z = leafPath(dir, leaf, &path) orelse return;
    _ = linux.unlink(z.ptr);
}

fn renameLeaf(dir: []const u8, from: []const u8, to: []const u8) bool {
    var a: [linux.PATH_MAX]u8 = undefined;
    var b: [linux.PATH_MAX]u8 = undefined;
    const za = leafPath(dir, from, &a) orelse return false;
    const zb = leafPath(dir, to, &b) orelse return false;
    return linux.errno(linux.renameat(linux.AT.FDCWD, za.ptr, linux.AT.FDCWD, zb.ptr)) == .SUCCESS;
}

fn siblingLeaf(mark: *const Mark, prefix: []const u8, out: *Mark) bool {
    const leaf = mark.slice();
    const body = if (std.mem.startsWith(u8, leaf, ".reg-"))
        leaf[".reg-".len..]
    else if (std.mem.startsWith(u8, leaf, ".wd-"))
        leaf[".wd-".len..]
    else
        return false;
    const text = std.fmt.bufPrint(&out.leaf, "{s}{s}", .{ prefix, body }) catch return false;
    out.len = text.len;
    return true;
}

/// Write the time and checksum, then rename `.reg` to `.wd`. The rename is
/// the publication: a reader never meets a `.wd` without its record.
fn withdraw(dir: []const u8, mark: *const Mark) bool {
    var path: [linux.PATH_MAX]u8 = undefined;
    const z = leafPath(dir, mark.slice(), &path) orelse return false;
    const fd_rc = linux.open(z.ptr, .{ .ACCMODE = .WRONLY, .CLOEXEC = true }, 0);
    if (linux.errno(fd_rc) != .SUCCESS) return false;
    const fd: linux.fd_t = @intCast(fd_rc);
    const rec = encodeWithdrawal(clock.boottimeNs());
    const n = linux.write(fd, &rec, rec.len);
    _ = linux.close(fd);
    if (linux.errno(n) != .SUCCESS or n != rec.len) return false;
    var wd: Mark = .{};
    if (!siblingLeaf(mark, ".wd-", &wd)) return false;
    return renameLeaf(dir, mark.slice(), wd.slice());
}

fn removeSection(dir: []const u8, mark: *const Mark) void {
    unlinkLeaf(dir, mark.slice());
}

fn removeWithdrawal(dir: []const u8, mark: *const Mark) void {
    var wd: Mark = .{};
    if (!siblingLeaf(mark, ".wd-", &wd)) return;
    unlinkLeaf(dir, wd.slice());
}

/// `.wd` → `.abandoned`, under the registration lock (the caller holds it,
/// or `abandon` takes it).
fn abandon(dir: []const u8, mark: *const Mark) void {
    var wd: Mark = .{};
    var ab: Mark = .{};
    if (!siblingLeaf(mark, ".wd-", &wd)) return;
    if (!siblingLeaf(mark, ".abandoned-", &ab)) return;
    _ = renameLeaf(dir, wd.slice(), ab.slice());
}

fn readFile(path_z: [*:0]const u8, buf: []u8) ?[]const u8 {
    return proc.readSmall(path_z, buf);
}

fn list(dir: []const u8, now_mono: u64, now_boot: u64, out: []Listed) []Listed {
    var path: [linux.PATH_MAX]u8 = undefined;
    const z = std.fmt.bufPrintSentinel(&path, "{s}", .{dir}, 0) catch return out[0..0];
    const rc = linux.open(z.ptr, .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .CLOEXEC = true }, 0);
    if (linux.errno(rc) != .SUCCESS) return out[0..0];
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    var boot_buf: [64]u8 = undefined;
    const boot = bootText(&boot_buf);
    var buf: [8192]u8 = undefined;
    var n: usize = 0;
    while (n < out.len) {
        const got = linux.getdents64(fd, &buf, buf.len);
        if (linux.errno(got) != .SUCCESS or got == 0) break;
        var off: usize = 0;
        while (off < got and n < out.len) {
            const reclen = std.mem.readInt(u16, buf[off + 16 ..][0..2], .little);
            if (reclen == 0) break;
            const name_at = off + 19;
            var name_end = name_at;
            const rec_end = off + reclen;
            while (name_end < rec_end and buf[name_end] != 0) name_end += 1;
            const name = buf[name_at..name_end];
            if (parseLeaf(name)) |p| {
                var aged = false;
                if (p.kind == .withdrawal) {
                    var file_buf: [32]u8 = undefined;
                    var fp: [linux.PATH_MAX]u8 = undefined;
                    if (leafPath(dir, name, &fp)) |fz| {
                        const bytes = readFile(fz.ptr, &file_buf) orelse "";
                        aged = withdrawalAged(bytes, now_boot);
                    } else aged = true;
                    if (aged and alive(p.pid, p.ticks) and claimAged(p.pid, p.ticks, p.seq)) noteAged(p.pid);
                }
                const live = p.boot.len == boot.len and std.mem.eql(u8, p.boot, boot) and alive(p.pid, p.ticks);
                var row: Listed = .{
                    .kind = p.kind,
                    .pid = p.pid,
                    .start_ticks = p.ticks,
                    .volume = p.vol,
                    .sequence = p.seq,
                    .boot_ok = p.boot.len == boot.len and std.mem.eql(u8, p.boot, boot),
                    .alive = live,
                    .aged = aged,
                };
                const copy = name[0..@min(name.len, row.leaf.len)];
                @memcpy(row.leaf[0..copy.len], copy);
                row.leaf_len = copy.len;
                out[n] = row;
                n += 1;
            }
            off += reclen;
        }
    }
    _ = now_mono;
    return out[0..n];
}

fn tally(rows: []const Listed, now_mono: u64, under_lock: bool) Survey {
    var s: Survey = .{};
    for (rows) |r| {
        if (r.kind == .abandoned or r.aged) {
            s.abandoned += 1;
            continue;
        }
        if (!r.alive) {
            if (r.kind == .section) {
                const at = firstSeen(r.pid, r.start_ticks, r.sequence, now_mono);
                if (now_mono -| at < NVME_IO_TIMEOUT_S * std.time.ns_per_s) s.dead_waiting += 1 else s.dead_ready += 1;
            } else s.dead_ready += 1;
            continue;
        }
        if (r.kind == .section) {
            s.live_section += 1;
            if (under_lock) marker_live_reg_locked.fire("a live section marker was visible under the registration lock");
        } else if (r.kind == .withdrawal) s.live_withdrawal += 1;
    }
    return s;
}

pub fn survey(dir: []const u8, under_lock: bool) Survey {
    var rows: [32]Listed = undefined;
    const got = list(dir, clock.monotonicNs(), clock.boottimeNs(), &rows);
    return tally(got, clock.monotonicNs(), under_lock);
}

pub fn forbids(dir: []const u8, role: Role) bool {
    const s = survey(dir, true);
    return switch (role) {
        .mutation => s.forbidsMutation(),
        .lapse_acquire => s.forbidsLapse(),
    };
}

/// Drop dead and abandoned markers of `volume` whose wait is over. Live
/// markers stay. The caller holds the registration lock.
pub fn removeJudged(dir: []const u8, volume: u32) void {
    var rows: [32]Listed = undefined;
    const now = clock.monotonicNs();
    const got = list(dir, now, clock.boottimeNs(), &rows);
    for (got) |r| {
        if (r.volume != volume) continue;
        const ready = r.kind == .abandoned or r.aged or (!r.alive and r.kind != .section) or (!r.alive and r.kind == .section and now -| firstSeen(r.pid, r.start_ticks, r.sequence, now) >= NVME_IO_TIMEOUT_S * std.time.ns_per_s);
        if (!ready) continue;
        unlinkLeaf(dir, r.leaf[0..r.leaf_len]);
    }
}

// The registry directory is a `[]const u8`. These wrappers keep the rule
// and the section on that type without a second import cycle.
pub fn createIn(dir: []const u8, volume: u32) error{ MarkerExists, Io }!Mark {
    return createSection(dir, volume);
}

pub fn dropSection(dir: []const u8, mark: *const Mark) void {
    removeSection(dir, mark);
}

pub fn publishWithdrawal(dir: []const u8, mark: *const Mark) bool {
    return withdraw(dir, mark);
}

pub fn dropWithdrawal(dir: []const u8, mark: *const Mark) void {
    removeWithdrawal(dir, mark);
}

pub fn abandonWithdrawal(dir: []const u8, mark: *const Mark) void {
    abandon(dir, mark);
}

test "marker withdraw span is T_withdraw + one I/O timeout + one beat, and it assumes io_timeout ≤ 30 s" {
    try std.testing.expectEqual(T_WITHDRAW_S + NVME_IO_TIMEOUT_S + claim.HEARTBEAT_INTERVAL_S, MARKER_WITHDRAW_MAX_S);
    try std.testing.expectEqual(@as(u64, 50), MARKER_WITHDRAW_MAX_S);
    try std.testing.expect(NVME_IO_TIMEOUT_S == 30);
}

test "marker names parse from the right, so a dashed boot id is not four fields" {
    const name = ".reg-550e8400-e29b-41d4-a716-446655440000-42-99-3-7";
    const p = parseLeaf(name) orelse return error.Parse;
    try std.testing.expectEqual(Kind.section, p.kind);
    try std.testing.expectEqualStrings("550e8400-e29b-41d4-a716-446655440000", p.boot);
    try std.testing.expectEqual(@as(u32, 42), p.pid);
    try std.testing.expectEqual(@as(u64, 99), p.ticks);
    try std.testing.expectEqual(@as(u32, 3), p.vol);
    try std.testing.expectEqual(@as(u64, 7), p.seq);
    const wd = parseLeaf(".wd-550e8400-e29b-41d4-a716-446655440000-42-99-3-7") orelse return error.Parse;
    try std.testing.expectEqual(Kind.withdrawal, wd.kind);
    const ab = parseLeaf(".abandoned-550e8400-e29b-41d4-a716-446655440000-42-99-3-7") orelse return error.Parse;
    try std.testing.expectEqual(Kind.abandoned, ab.kind);
    try std.testing.expect(parseLeaf(".lock") == null);
}

test "a withdrawal record that fails its checksum is abandoned, and one inside the span is not" {
    const rec = encodeWithdrawal(1_000);
    try std.testing.expect(!withdrawalAged(&rec, 1_000 + 10 * std.time.ns_per_s));
    try std.testing.expect(withdrawalAged(&rec, 1_000 + (MARKER_WITHDRAW_MAX_S + 1) * std.time.ns_per_s));
    var bad = rec;
    bad[0] ^= 1;
    try std.testing.expect(withdrawalAged(&bad, 1_000));
    try std.testing.expect(withdrawalAged(rec[0..4], 1_000));
}

test "marker tally: a live section under the lock trips, a live withdrawal refuses both roles, a dead section waits out one I/O timeout, an abandoned marker refuses mutations only" {
    const rows = [_]Listed{
        .{ .kind = .section, .pid = 1, .start_ticks = 1, .volume = 0, .sequence = 1, .boot_ok = true, .alive = true, .aged = false, .leaf_len = 0 },
    };
    log.testing.expectErrorLogs(1);
    const live = tally(&rows, 0, true);
    try log.testing.expectAllFired();
    try std.testing.expect(live.forbidsMutation() and live.forbidsLapse());
    try std.testing.expect(!live.needsCheck());

    const wd = [_]Listed{
        .{ .kind = .withdrawal, .pid = 1, .start_ticks = 1, .volume = 0, .sequence = 1, .boot_ok = true, .alive = true, .aged = false, .leaf_len = 0 },
    };
    const w = tally(&wd, 0, true);
    try std.testing.expect(w.forbidsMutation() and w.forbidsLapse());

    const dead = [_]Listed{
        .{ .kind = .section, .pid = 9, .start_ticks = 9, .volume = 0, .sequence = 9, .boot_ok = true, .alive = false, .aged = false, .leaf_len = 0 },
    };
    const waiting = tally(&dead, 0, false);
    try std.testing.expect(waiting.forbidsMutation() and waiting.forbidsLapse());
    try std.testing.expect(!waiting.needsCheck());
    const ready = tally(&dead, NVME_IO_TIMEOUT_S * std.time.ns_per_s, false);
    try std.testing.expect(ready.forbidsMutation() and !ready.forbidsLapse() and ready.needsCheck());

    const ab = [_]Listed{
        .{ .kind = .abandoned, .pid = 3, .start_ticks = 3, .volume = 1, .sequence = 1, .boot_ok = true, .alive = false, .aged = false, .leaf_len = 0 },
    };
    const a = tally(&ab, 0, false);
    try std.testing.expect(a.forbidsMutation() and !a.forbidsLapse() and a.needsCheck());
}
