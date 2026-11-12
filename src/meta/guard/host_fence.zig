//! Per-host fencing (owner ruling 2026-11-02, option C; decision 0003 §DF;
//! charter §5.5 "Fencing"): every mount of a set on one host presents ONE
//! NVMe host identity and ONE registration key — derived from the WIRE host
//! id the namespace sees (FID 0x81) and the set — so the device fences the
//! HOST (the GPFS node model): a preempt of the key fences every mount of
//! the set on the host, and a registration admits every one of them.
//!
//! The safety rule this module owns: NVMe keeps a registration per host
//! identifier, so a registration that admits the host afresh (the report
//! holds none of our wire host id under our key — a first mount, a fenced
//! host, a lapse) re-admits every process of the host. Before it, every
//! STALE sibling — another live incarnation of the set's mounts on this host
//! whose region a recoverer took (a page Recovering, Recovered or Free that
//! no retirement covers; the manager superseded on appender 0) — is proven
//! dead by the §DA writer-id proof or killed through a pidfd and its exit
//! awaited, then its writes drained off the namespace's block queue; one
//! that can be neither refuses the registration loud by name. After the
//! register, still under the lock, the pages are judged AGAIN: a sibling a
//! remote recovery turned stale meanwhile takes the host's registration
//! away again before it is killed (review B2). A CURRENT sibling (a Live
//! page names its incarnation), one whose every non-Live page is covered by
//! its own retirement declaration (`r <writer id> <appender id> <term>`:
//! §BS's re-joiner — the page's term, since appender ids are reused) and a
//! joiner no page names yet are spared: none holds a region a recoverer
//! took. A manager registered whose role is not durable yet (`p`) is STALE
//! whenever the rule runs: the rule runs only when this host is not
//! registered, and that manager registered it under the lock — the host was
//! preempted since, the manager superseded.
//!
//! The candidates come from the host's registry of the set's mounts
//! (`<run>/hosts/set-<key>/<pid>`, keyed by the set and the wire host id: a
//! mount under another identity is another host, never a sibling); the same
//! registry shape counts the users of a shared fabric association
//! (`fabric-<key>`). Every mutation and every judgement runs under the
//! registry's flock, taken with a bounded wait that fails loud. A process
//! outside the initial pid namespace never enters a registry (its pids are
//! not the ones its siblings judge): its PR posture is refused. Linux only.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;
const Io = std.Io;

const log = @import("../../core/log.zig");
const knobs = @import("../../core/knobs.zig");
const checksum = @import("../../core/checksum.zig");
const clock = @import("../../core/clock.zig");
const journal_tail = @import("../journal_tail.zig");
const guard = @import("../guard.zig");
const claim = @import("claim.zig");
const pr = @import("pr.zig");
const nvme = @import("nvme_cmd.zig");
const proc = @import("proc.zig");
const pr_linux = @import("pr_linux.zig");
const page = @import("../sym/appender_page.zig");
const fencing = @import("../sym/membership/fencing.zig");
const initiator = @import("../../ops/nvmet/initiator.zig");
const marker = @import("marker.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("guard/host_fence is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

/// Test seam: every mount presents its OWN identity (the derived hostid
/// salted by the mount point), its own per-incarnation key, and the rule
/// never runs — the venue's single-host rows that need two hosts' fencing.
/// Works because the venue boots `nvme_core.multipath=N`.
pub fn perMountIdentity(env: *const std.process.Environ.Map) bool {
    return knobs.boolKnob("SQUEEZEFS_TEST_PER_MOUNT_IDENTITY", env);
}

/// Test seam: a stale sibling's kill is refused as if the kernel denied it
/// (the loud refusal's row).
fn killRefusedSeam(env: *const std.process.Environ.Map) bool {
    return knobs.boolKnob("SQUEEZEFS_TEST_SIBLING_KILL_REFUSED", env);
}

/// The set's registration key on a host: the wire host id (the device's
/// own name for the host, FID 0x81) and the set — stable across remounts
/// and reboots, the same for every mount of the set on the host, different
/// for another host or another set. Never 0 (0 = no key).
pub fn hostKey(wire_host_id: []const u8, set_uuid: [16]u8) u64 {
    var h: checksum.Hasher = .init();
    h.update("squeezefs:pr-host-key");
    h.update(wire_host_id);
    h.update(&[_]u8{0});
    h.update(&set_uuid);
    const k = h.finalLow64();
    return if (k == 0) 1 else k;
}

/// A joiner's claimed page key read against the joined volume's report
/// (decision 0004 §6.1, KD18).
pub const JoinKeyVerdict = enum { ok, absent, mismatch };

/// Host-keyed: a registrant holds non-zero `claimed` under a host id that
/// derives it via `hostKey(id, set)`. Incarnation-keyed: holding it suffices.
/// The key belongs to some registered host, not necessarily the requester
/// (decision 0003 §DO.28).
pub fn joinKeyVerdict(rep: nvme.Report, claimed: u64, set_uuid: [16]u8, host_keyed: bool) JoinKeyVerdict {
    if (claimed == 0) return .absent;
    var held = false;
    for (rep.registrants) |r| {
        if (r.rkey != claimed) continue;
        held = true;
        if (!host_keyed or hostKey(&r.host_id, set_uuid) == claimed) return .ok;
    }
    return if (held) .mismatch else .absent;
}

/// The registry key of the set's mounts under one wire host id.
pub fn setRegistryKey(wire_host_id: []const u8, set_uuid: [16]u8) u64 {
    var h: checksum.Hasher = .init();
    h.update("squeezefs:host-set");
    h.update(wire_host_id);
    h.update(&[_]u8{0});
    h.update(&set_uuid);
    return h.finalLow64();
}

/// The registry key of one fabric association (a subsystem under one
/// host identity).
pub fn fabricRegistryKey(subnqn: []const u8, hostnqn: []const u8, hostid: []const u8) u64 {
    var h: checksum.Hasher = .init();
    h.update("squeezefs:host-fabric");
    h.update(subnqn);
    h.update(&[_]u8{0});
    h.update(hostnqn);
    h.update(&[_]u8{0});
    h.update(hostid);
    return h.finalLow64();
}

// ---------------------------------------------------------------------------
// The registry.
// ---------------------------------------------------------------------------

pub const Kind = enum {
    set,
    fabric,

    fn prefix(self: Kind) []const u8 {
        return switch (self) {
            .set => "set",
            .fabric => "fabric",
        };
    }

    /// The registry's lock wait: the set's covers one rule (a kill's 10 s
    /// wait and a drain's), the fabric's one connect (the kernel's
    /// `nvme_core.admin_timeout`, 60 s, bounds a namespace scan).
    fn lockWaitMs(self: Kind) u64 {
        return switch (self) {
            .set => 30_000, // two KILL_WAIT_MS + the drain bound, with room
            .fabric => 120_000, // twice nvme_core.admin_timeout's 60 s default: one connect under the lock
        };
    }
};

/// `m` a manager whose role is durable (appender 0's page, or the claim on a
/// flat volume; its `p` line retired then), `p` a manager registered whose
/// role is not yet, `j` a joiner, `l` a joiner leaving (its leave's writes
/// still to land).
pub const Role = enum { manager, pending, joiner, leaving };

/// A joined region's page (review 4 blocker 1): the appender id and term
/// its `JoinAppender` answered, on the volume it joined.
pub const Joined = struct { appender_id: u32, term: u64, vol: u32 };

/// One region an entry's process holds (its writer id at its slot; a
/// joiner's page once joined).
pub const Region = struct { role: Role, slot: u32, writer_id: u64, joined: ?Joined = null };

/// One retired incarnation's page: the writer id, the appender id and the
/// term its page carried when the latch failed (review B1: a §BS successor
/// keeps the writer id; a later successor may get the same appender id back
/// — the lowest Free pair — but never the same term), on its volume (a
/// member's volumes share its writer id and often its appender id: review
/// 5 minor 1).
pub const Retired = struct { writer_id: u64, appender_id: u32, term: u64, vol: u32 };

pub const MAX_REGIONS = 16; // one region per metadata volume a process writes: the set's member cap

pub const Entry = struct {
    pid: u32,
    start_ticks: u64,
    /// The writer's node token (decision 0004 §4.4.2, KD19): what the entry's
    /// process judges pages by. 0 = the entry carries none, which is
    /// unjudgeable, not a token.
    node_token: u64 = 0,
    regions: [MAX_REGIONS]Region = undefined,
    n_regions: usize = 0,
    retired: [MAX_REGIONS]Retired = undefined,
    n_retired: usize = 0,
    /// The manager's re-acquire mark (decision 0004 DC24): `CLOCK_BOOTTIME`
    /// of the acquire, and the reservation generation it answered. 0 = none.
    reacquire_ns: u64 = 0,
    reacquire_gen: u32 = 0,

    pub fn regionSlice(self: *const Entry) []const Region {
        return self.regions[0..self.n_regions];
    }

    /// A re-acquire mark written at or after `since_ns` (decision 0004 DC24).
    /// The re-read's boottime is `since_ns`; a mark from before it does not
    /// cover an acquire that landed after the re-read.
    pub fn markedSince(self: *const Entry, since_ns: u64) bool {
        return self.reacquire_ns != 0 and self.reacquire_ns >= since_ns;
    }

    fn covers(self: *const Entry, p: page.Page, vol: u32) bool {
        return self.coversId(p.identity.writer_id, p.appender_id, p.term, vol);
    }

    fn coversId(self: *const Entry, writer_id: u64, appender_id: u32, term: u64, vol: u32) bool {
        for (self.retired[0..self.n_retired]) |r| if (r.writer_id == writer_id and r.appender_id == appender_id and r.term == term and r.vol == vol) return true;
        return false;
    }

    /// The entry's text: the header line — this process's start ticks and its
    /// node token, hex (an entry of an older binary has no token word, which
    /// is `.unjudgeable`, never a token of 0) — then `m|p|j|l <slot>
    /// <writer id>` per region — a joined `j`/`l` line adds `<appender id>
    /// <term> <volume>` —, `r <writer id> <appender id> <term>` per retired
    /// incarnation (an `r` line short of a word retires nothing; a region
    /// line short of its page's words names no page).
    pub fn parse(pid: u32, text: []const u8) ?Entry {
        var lines = std.mem.tokenizeScalar(u8, text, '\n');
        const first = lines.next() orelse return null;
        var hf = std.mem.tokenizeScalar(u8, first, ' ');
        var e: Entry = .{ .pid = pid, .start_ticks = std.fmt.parseInt(u64, hf.next() orelse return null, 10) catch return null }; // proc(5)'s start ticks, decimal
        e.node_token = std.fmt.parseInt(u64, hf.next() orelse "", 16) catch 0; // the node token, hex as `append` writes it
        while (lines.next()) |line| {
            var f = std.mem.tokenizeScalar(u8, line, ' ');
            const tag = f.next() orelse continue;
            if (std.mem.eql(u8, tag, "a")) {
                e.reacquire_ns = std.fmt.parseInt(u64, f.next() orelse continue, 10) catch continue;
                e.reacquire_gen = std.fmt.parseInt(u32, f.next() orelse "0", 10) catch 0;
                continue;
            }
            if (std.mem.eql(u8, tag, "r")) {
                const w = std.fmt.parseInt(u64, f.next() orelse continue, 16) catch continue; // a writer id, hex as `retiredLine` writes it
                const a = std.fmt.parseInt(u32, f.next() orelse continue, 10) catch continue; // an appender id, decimal as `retiredLine` writes it
                const t = std.fmt.parseInt(u64, f.next() orelse continue, 10) catch continue; // the page's term, decimal as `retiredLine` writes it
                const v = std.fmt.parseInt(u32, f.next() orelse continue, 10) catch continue; // the volume's lane index, decimal as `retiredLine` writes it
                if (e.n_retired < MAX_REGIONS) {
                    e.retired[e.n_retired] = .{ .writer_id = w, .appender_id = a, .term = t, .vol = v };
                    e.n_retired += 1;
                }
                continue;
            }
            const role: Role = if (std.mem.eql(u8, tag, "m")) .manager else if (std.mem.eql(u8, tag, "p")) .pending else if (std.mem.eql(u8, tag, "j")) .joiner else if (std.mem.eql(u8, tag, "l")) .leaving else continue;
            const slot = std.fmt.parseInt(u32, f.next() orelse continue, 10) catch continue; // a mount slot, decimal as `regionLine` writes it
            const w = std.fmt.parseInt(u64, f.next() orelse continue, 16) catch continue; // a writer id, hex as `regionLine` writes it
            const joined: ?Joined = blk: {
                if (role != .joiner and role != .leaving) break :blk null;
                const a = std.fmt.parseInt(u32, f.next() orelse break :blk null, 10) catch break :blk null; // an appender id, decimal as `regionLine` writes it
                const t = std.fmt.parseInt(u64, f.next() orelse break :blk null, 10) catch break :blk null; // the page's term, decimal as `regionLine` writes it
                const v = std.fmt.parseInt(u32, f.next() orelse break :blk null, 10) catch break :blk null; // the volume's lane index, decimal as `regionLine` writes it
                break :blk .{ .appender_id = a, .term = t, .vol = v };
            };
            if (e.n_regions < MAX_REGIONS) {
                e.regions[e.n_regions] = .{ .role = role, .slot = slot, .writer_id = w, .joined = joined };
                e.n_regions += 1;
            }
        }
        return e;
    }
};

pub const RegistryError = error{RegistryUnavailable};

/// This thread's registration-lock depth. The registry flock must not be
/// taken under it (decision 0004 DC25: flock, then the registration lock).
threadlocal var registration_depth: u32 = 0;

/// One acquisition of the registration lock. Release closes its file
/// description, which drops the flock.
pub const RegistrationHold = struct {
    fd: linux.fd_t = -1,

    pub fn release(self: *RegistrationHold) void {
        if (self.fd < 0) return;
        if (registration_depth > 0) registration_depth -= 1;
        _ = linux.close(self.fd);
        self.fd = -1;
    }
};

/// A registry directory under the run dir (`/run/squeezefs/hosts` as root),
/// one file per process, one flock serializing every mutation and
/// judgement.
pub const Registry = struct {
    dir_buf: [linux.PATH_MAX]u8 = undefined,
    dir_len: usize = 0,
    kind: Kind = .set,
    lock_fd: linux.fd_t = -1,
    /// Test seam: every append fails (a full or unwritable run dir).
    test_fail_append: if (builtin.is_test) bool else void = if (builtin.is_test) false else {},

    pub fn at(base: []const u8, kind: Kind, key: u64) Registry {
        var r: Registry = .{ .kind = kind };
        r.dir_len = (std.fmt.bufPrint(&r.dir_buf, "{s}/{s}-{x:0>16}", .{ base, kind.prefix(), key }) catch unreachable).len;
        return r;
    }

    /// Under the run dir; `SQUEEZEFS_TEST_HOST_REGISTRY_DIR` moves the
    /// root (a test rig's pid-tagged dir: its rows leave nothing under the
    /// real `/run`, and every process of the rig shares it).
    pub fn runtime(env: *const std.process.Environ.Map, kind: Kind, key: u64) Registry {
        if (knobs.parseStr(env.get("SQUEEZEFS_TEST_HOST_REGISTRY_DIR"))) |root| if (!std.mem.eql(u8, root, "off")) return at(root, kind, key);
        const d = journal_tail.HeldDir.runtimeOf(.hosts);
        return at(d.slice(), kind, key);
    }

    pub fn dir(self: *const Registry) []const u8 {
        return self.dir_buf[0..self.dir_len];
    }

    fn pathOf(self: *const Registry, buf: *[linux.PATH_MAX]u8, leaf: []const u8) [:0]const u8 {
        return std.fmt.bufPrintSentinel(buf, "{s}/{s}", .{ self.dir(), leaf }, 0) catch unreachable;
    }

    fn entryPath(self: *const Registry, buf: *[linux.PATH_MAX]u8, pid: u32) [:0]const u8 {
        var nb: [24]u8 = undefined;
        return self.pathOf(buf, std.fmt.bufPrint(&nb, "{d}", .{pid}) catch unreachable);
    }

    fn mkdirs(self: *const Registry) RegistryError!void {
        var buf: [linux.PATH_MAX]u8 = undefined;
        const d = self.dir();
        var i: usize = 1;
        while (i <= d.len) : (i += 1) {
            if (i != d.len and d[i] != '/') continue;
            const p = std.fmt.bufPrintSentinel(&buf, "{s}", .{d[0..i]}, 0) catch return error.RegistryUnavailable;
            const rc = linux.mkdir(p.ptr, 0o700); // root's run state: no other user reads the host's registries
            const e = linux.errno(rc);
            if (e != .SUCCESS and e != .EXIST) return error.RegistryUnavailable;
        }
    }

    /// The registry's flock, waited for at most the kind's bound (review
    /// major 10: a SIGSTOPped holder never wedges every co-located mount):
    /// `LOCK_NB` retried on a doubling backoff; past the bound, refused.
    pub fn lock(self: *Registry) RegistryError!void {
        if (registration_depth != 0) log.err("host registry {s}: the registry flock was taken while this thread holds the registration lock — the order is flock, then registration (decision 0004 DC25)", .{self.dir()});
        try self.mkdirs();
        var buf: [linux.PATH_MAX]u8 = undefined;
        const p = self.pathOf(&buf, ".lock");
        const rc = linux.open(p.ptr, .{ .ACCMODE = .RDWR, .CREAT = true, .CLOEXEC = true }, 0o600);
        if (linux.errno(rc) != .SUCCESS) return error.RegistryUnavailable;
        const fd: linux.fd_t = @intCast(rc);
        const deadline = clock.monotonicNs() +| self.kind.lockWaitMs() * std.time.ns_per_ms;
        var backoff_ns: u64 = std.time.ns_per_ms;
        while (true) {
            const lr = linux.flock(fd, std.posix.LOCK.EX | std.posix.LOCK.NB);
            switch (linux.errno(lr)) {
                .SUCCESS => break,
                .INTR => continue,
                .AGAIN => {
                    const now = clock.monotonicNs();
                    if (now >= deadline) {
                        _ = linux.close(fd);
                        log.err("host registry {s}: its lock is held past {d} ms (a stopped or wedged sibling holds it) — refused", .{ self.dir(), self.kind.lockWaitMs() });
                        return error.RegistryUnavailable;
                    }
                    const ts: linux.timespec = .{ .sec = 0, .nsec = @intCast(@min(backoff_ns, deadline - now)) };
                    _ = linux.nanosleep(&ts, null);
                    backoff_ns = @min(backoff_ns * 2, 100 * std.time.ns_per_ms); // a lock poll's ceiling: a held rule lasts seconds
                },
                else => {
                    _ = linux.close(fd);
                    return error.RegistryUnavailable;
                },
            }
        }
        self.lock_fd = fd;
    }

    /// The exclusive flock holder of `.lock` or `.register`, with its start
    /// ticks. Null when the census names none (a tear reads as unnamed, and
    /// the frozen-joiner count resets).
    pub fn flockHolder(self: *const Registry, which: LockFile) ?marker.HolderId {
        var buf: [linux.PATH_MAX]u8 = undefined;
        const leaf: []const u8 = switch (which) {
            .registry => ".lock",
            .registration => ".register",
        };
        const found = proc.exclusiveLocker(self.pathOf(&buf, leaf)) orelse return null;
        if (found.pid <= 0) return null;
        const ticks = switch (proc.startTicksAnyState(@intCast(found.pid))) {
            .ticks => |t| t,
            else => return null,
        };
        return .{ .pid = @intCast(found.pid), .start_ticks = ticks };
    }

    pub fn unlock(self: *Registry) void {
        if (self.lock_fd < 0) return;
        _ = linux.close(self.lock_fd);
        self.lock_fd = -1;
    }

    /// The set's registration lock (decision 0004 DC20): `<dir>/.register`,
    /// beside the registry flock's `.lock`. Each acquisition opens its own
    /// file description, so two threads of one process exclude each other
    /// (a flock excludes only separate descriptions). `wait_ms` 0 is the
    /// production bound. Closing the description releases the lock.
    pub fn lockRegistration(self: *const Registry, wait_ms: u64) RegistryError!RegistrationHold {
        try self.mkdirs();
        var buf: [linux.PATH_MAX]u8 = undefined;
        const p = self.pathOf(&buf, ".register");
        const rc = linux.open(p.ptr, .{ .ACCMODE = .RDWR, .CREAT = true, .CLOEXEC = true }, 0o600);
        if (linux.errno(rc) != .SUCCESS) return error.RegistryUnavailable;
        const fd: linux.fd_t = @intCast(rc);
        const bound_ms = if (wait_ms == 0) REGISTRATION_LOCK_WAIT_MS else wait_ms;
        const deadline = clock.monotonicNs() +| bound_ms * std.time.ns_per_ms;
        var backoff_ns: u64 = std.time.ns_per_ms;
        while (true) {
            const lr = linux.flock(fd, std.posix.LOCK.EX | std.posix.LOCK.NB);
            switch (linux.errno(lr)) {
                .SUCCESS => {
                    registration_depth += 1;
                    return .{ .fd = fd };
                },
                .INTR => continue,
                .AGAIN => {
                    const now = clock.monotonicNs();
                    if (now >= deadline) {
                        _ = linux.close(fd);
                        log.err("host registry {s}: its registration lock is held past {d} ms (a stopped or wedged section holds it) — refused", .{ self.dir(), bound_ms });
                        return error.RegistryUnavailable;
                    }
                    const ts: linux.timespec = .{ .sec = 0, .nsec = @intCast(@min(backoff_ns, deadline - now)) };
                    _ = linux.nanosleep(&ts, null);
                    backoff_ns = @min(backoff_ns * 2, 100 * std.time.ns_per_ms);
                },
                else => {
                    _ = linux.close(fd);
                    return error.RegistryUnavailable;
                },
            }
        }
    }

    /// This process's entry text (null = none).
    fn readEntry(self: *const Registry, pid: u32, buf: []u8) ?[]const u8 {
        var pb: [linux.PATH_MAX]u8 = undefined;
        return proc.readSmall(self.entryPath(&pb, pid).ptr, buf);
    }

    /// Rewrite this process's entry whole (tmp + rename): `text` empty =
    /// removed.
    fn writeEntry(self: *const Registry, pid: u32, text: []const u8) RegistryError!void {
        var pb: [linux.PATH_MAX]u8 = undefined;
        const path = self.entryPath(&pb, pid);
        if (text.len == 0) {
            _ = linux.unlink(path.ptr);
            return;
        }
        var tb: [linux.PATH_MAX]u8 = undefined;
        const tmp = std.fmt.bufPrintSentinel(&tb, "{s}.tmp", .{path}, 0) catch return error.RegistryUnavailable;
        const rc = linux.open(tmp.ptr, .{ .ACCMODE = .WRONLY, .CREAT = true, .TRUNC = true, .CLOEXEC = true }, 0o600);
        if (linux.errno(rc) != .SUCCESS) return error.RegistryUnavailable;
        const fd: linux.fd_t = @intCast(rc);
        writeAll(fd, text) catch {
            _ = linux.close(fd);
            return error.RegistryUnavailable;
        };
        _ = linux.close(fd);
        if (linux.errno(linux.rename(tmp.ptr, path.ptr)) != .SUCCESS) return error.RegistryUnavailable;
    }

    /// Append `line` to this process's entry. The entry's start ticks are
    /// this process's: a file a crashed predecessor under a reused pid left
    /// is replaced, never extended (review major 10). The header carries this
    /// process's node token, so a sibling can tell whether it can judge this
    /// entry at all (decision 0004 §4.4.2, KD19).
    pub fn append(self: *const Registry, pid: u32, start_ticks: u64, node_token: u64, line: []const u8) RegistryError!void {
        if (builtin.is_test and self.test_fail_append) return error.RegistryUnavailable;
        try self.mkdirs();
        var rb: [4096]u8 = undefined;
        var out: [4096 + 128]u8 = undefined;
        var w: Io.Writer = .fixed(&out);
        const old = self.readEntry(pid, &rb);
        const ours = if (old) |t| if (Entry.parse(pid, t)) |e| e.start_ticks == start_ticks else false else false;
        if (ours) w.writeAll(old.?) catch return error.RegistryUnavailable else w.print("{d} {x:0>16}\n", .{ start_ticks, node_token }) catch return error.RegistryUnavailable;
        w.writeAll(line) catch return error.RegistryUnavailable;
        try self.writeEntry(pid, w.buffered());
    }

    /// The manager's re-acquire mark (decision 0004 DC24), replacing any
    /// mark already on the line. The caller holds the registration lock.
    /// `at_ns` is `CLOCK_BOOTTIME` of the acquire; `generation` is the
    /// reservation generation it answered.
    pub fn noteReacquire(self: *const Registry, pid: u32, start_ticks: u64, node_token: u64, at_ns: u64, generation: u32) RegistryError!void {
        var rb: [4096]u8 = undefined;
        var out: [4096]u8 = undefined;
        var w: Io.Writer = .fixed(&out);
        const old = self.readEntry(pid, &rb);
        const ours = if (old) |t| if (Entry.parse(pid, t)) |e| e.start_ticks == start_ticks else false else false;
        if (ours) {
            var lines = std.mem.tokenizeScalar(u8, old.?, '\n');
            const header = lines.next() orelse return error.RegistryUnavailable;
            w.print("{s}\n", .{header}) catch return error.RegistryUnavailable;
            while (lines.next()) |line| {
                if (std.mem.startsWith(u8, line, "a ")) continue;
                w.print("{s}\n", .{line}) catch return error.RegistryUnavailable;
            }
        } else w.print("{d} {x:0>16}\n", .{ start_ticks, node_token }) catch return error.RegistryUnavailable;
        w.print("a {d} {d}\n", .{ at_ns, generation }) catch return error.RegistryUnavailable;
        try self.writeEntry(pid, w.buffered());
    }

    /// Which of a writer id's lines `removeRegion` drops: all of them; a
    /// joiner's lines not yet naming a page; or those naming one appender id
    /// (its `j`/`l` line and its `r` retirement) — a re-joined successor
    /// keeps the writer id, its predecessor's lines go with the predecessor.
    pub const Which = union(enum) { any, unjoined, page: Page };
    pub const Page = struct { aid: u32, vol: u32 };

    /// Drop this process's lines naming `writer_id` under `tags` (one
    /// region's — review major 10: never the whole file while another
    /// region of the process stands); the file goes with its last line.
    pub fn removeRegion(self: *const Registry, pid: u32, tags: []const u8, writer_id: u64, which: Which) RegistryError!void {
        var rb: [4096]u8 = undefined;
        const old = self.readEntry(pid, &rb) orelse return;
        var out: [4096]u8 = undefined;
        var w: Io.Writer = .fixed(&out);
        var lines = std.mem.tokenizeScalar(u8, old, '\n');
        const header = lines.next() orelse return self.remove(pid);
        w.print("{s}\n", .{header}) catch return error.RegistryUnavailable;
        var kept: usize = 0;
        while (lines.next()) |line| {
            if (!dropsLine(line, tags, writer_id, which)) {
                w.print("{s}\n", .{line}) catch return error.RegistryUnavailable;
                // The re-acquire mark is not a region. The file goes when
                // the last region does.
                if (!std.mem.startsWith(u8, line, "a ")) kept += 1;
            }
        }
        try self.writeEntry(pid, if (kept == 0) "" else w.buffered());
    }

    fn dropsLine(line: []const u8, tags: []const u8, writer_id: u64, which: Which) bool {
        var f = std.mem.tokenizeScalar(u8, line, ' ');
        const tag = f.next() orelse return false;
        if (tag.len != 1 or std.mem.indexOfScalar(u8, tags, tag[0]) == null) return false;
        if (tag[0] != 'r') _ = f.next(); // the slot
        const w_text = f.next() orelse return false;
        if ((std.fmt.parseInt(u64, w_text, 16) catch return false) != writer_id) return false; // a writer id, hex as written
        // `r`: the appender id, term, volume next; a joined region line: its
        // page's appender id, term, volume.
        const aid: ?u32 = if (f.next()) |a| std.fmt.parseInt(u32, a, 10) catch null else null; // decimal as written
        _ = f.next(); // the term
        const vol: ?u32 = if (f.next()) |v| std.fmt.parseInt(u32, v, 10) catch null else null; // decimal as written
        return switch (which) {
            .any => true,
            .unjoined => tag[0] != 'r' and aid == null,
            .page => |want| aid == want.aid and vol == want.vol,
        };
    }

    /// Drop one exact line of this process's entry (the file goes with its
    /// last line); true = other lines of the process remain.
    pub fn removeLine(self: *const Registry, pid: u32, line: []const u8) RegistryError!bool {
        var rb: [4096]u8 = undefined;
        const old = self.readEntry(pid, &rb) orelse return false;
        var out: [4096]u8 = undefined;
        var w: Io.Writer = .fixed(&out);
        var lines = std.mem.tokenizeScalar(u8, old, '\n');
        const header = lines.next() orelse {
            self.remove(pid);
            return false;
        };
        w.print("{s}\n", .{header}) catch return error.RegistryUnavailable;
        const want = std.mem.trimEnd(u8, line, "\n");
        var kept: usize = 0;
        var dropped = false;
        while (lines.next()) |l| {
            if (!dropped and std.mem.eql(u8, l, want)) {
                dropped = true;
                continue;
            }
            w.print("{s}\n", .{l}) catch return error.RegistryUnavailable;
            kept += 1;
        }
        try self.writeEntry(pid, if (kept == 0) "" else w.buffered());
        return kept != 0;
    }

    /// LIVE entries other than `pid`'s holding `line` exactly (a dead
    /// process's file is pruned by `entries`).
    pub fn othersWithLine(self: *const Registry, gpa: Allocator, pid: u32, line: []const u8) (RegistryError || Allocator.Error)!usize {
        const es = try self.entries(gpa);
        defer gpa.free(es);
        const want = std.mem.trimEnd(u8, line, "\n");
        var n: usize = 0;
        for (es) |e| {
            if (e.pid == pid) continue;
            var tb: [4096]u8 = undefined;
            const text = self.readEntry(e.pid, &tb) orelse continue;
            var lines = std.mem.tokenizeScalar(u8, text, '\n');
            _ = lines.next(); // the start ticks
            while (lines.next()) |l| if (std.mem.eql(u8, l, want)) {
                n += 1;
                break;
            };
        }
        return n;
    }

    /// The whole entry (a dead process's, or a fabric association's user).
    pub fn remove(self: *const Registry, pid: u32) void {
        var pb: [linux.PATH_MAX]u8 = undefined;
        _ = linux.unlink(self.entryPath(&pb, pid).ptr);
    }

    /// The LIVE entries (`proc.incarnationAlive`: a thread of the process
    /// under its start ticks still runs, or its stat cannot be read; a dead
    /// one's file is removed here), this process's included. Raw
    /// `getdents64` (no `Io`: the D0 gate judges on the blocking pool).
    pub fn entries(self: *const Registry, gpa: Allocator) (RegistryError || Allocator.Error)![]Entry {
        var out: std.ArrayList(Entry) = .empty;
        errdefer out.deinit(gpa);
        var walk = initiator.DirWalk.open("{s}", .{self.dir()}) catch |e| switch (e) {
            error.FileNotFound => return out.toOwnedSlice(gpa),
            else => return error.RegistryUnavailable,
        };
        defer walk.close();
        while (walk.next() catch return error.RegistryUnavailable) |name| {
            const pid = std.fmt.parseInt(u32, name, 10) catch continue; // `.lock`, `.tmp` and strays
            var tb: [4096]u8 = undefined;
            const text = self.readEntry(pid, &tb) orelse continue;
            const e = Entry.parse(pid, text) orelse continue;
            if (!proc.incarnationAlive(pid, e.start_ticks)) {
                self.remove(pid);
                continue;
            }
            try out.append(gpa, e);
        }
        return out.toOwnedSlice(gpa);
    }

    /// Live entries other than `pid`.
    pub fn othersAlive(self: *const Registry, gpa: Allocator, pid: u32) (RegistryError || Allocator.Error)!usize {
        const es = try self.entries(gpa);
        defer gpa.free(es);
        var n: usize = 0;
        for (es) |e| {
            if (e.pid != pid) n += 1;
        }
        return n;
    }
};

fn writeAll(fd: linux.fd_t, bytes: []const u8) RegistryError!void {
    var off: usize = 0;
    while (off < bytes.len) {
        const rc = linux.write(fd, bytes[off..].ptr, bytes.len - off);
        switch (linux.errno(rc)) {
            .SUCCESS => off += rc,
            .INTR => continue,
            else => return error.RegistryUnavailable,
        }
    }
}

pub fn regionLine(buf: []u8, r: Region) []const u8 {
    const tag: []const u8 = switch (r.role) {
        .manager => "m",
        .pending => "p",
        .joiner => "j",
        .leaving => "l",
    };
    if (r.joined) |j| return std.fmt.bufPrint(buf, "{s} {d} {x:0>16} {d} {d} {d}\n", .{ tag, r.slot, r.writer_id, j.appender_id, j.term, j.vol }) catch unreachable;
    return std.fmt.bufPrint(buf, "{s} {d} {x:0>16}\n", .{ tag, r.slot, r.writer_id }) catch unreachable;
}

pub fn retiredLine(buf: []u8, r: Retired) []const u8 {
    return std.fmt.bufPrint(buf, "r {x:0>16} {d} {d} {d}\n", .{ r.writer_id, r.appender_id, r.term, r.vol }) catch unreachable;
}

// ---------------------------------------------------------------------------
// The judgement (pure).
// ---------------------------------------------------------------------------

pub const Verdict = enum {
    /// A Live page names its incarnation: its region is its own.
    current,
    /// Every non-Live page naming it is covered by its own retirement.
    retired,
    /// A joiner no page names yet: it holds no region a recoverer took.
    joining,
    /// A recoverer took one of its regions (or the manager role passed):
    /// it must be dead before the host is admitted.
    stale,
    /// Its entry's node token is not this process's, or it has none: every
    /// judgement here reads pages by node token, so this entry cannot be
    /// judged either way. A refusal, never a kill (decision 0004 §4.4.2).
    unjudgeable,
};

/// What the report says while the rule runs (this host unregistered).
pub const Facts = struct {
    /// A reservation stands (rtype ≠ 0). Under WEAR a reservation that
    /// stands while this host is unregistered means this host is not
    /// admitted, whatever the holder pointer names (decision 0004 §5.2).
    reserved: bool = false,
};

/// The verdict of one sibling's entry against one volume's pages (appender
/// 0 = the manager's; `vol_index` = the volume's lane index, a manager's
/// slot on it) and the report's `facts`. `taking_manager` = the registering
/// mount is winning the D0 role on this volume: appender 0's page names a
/// superseded holder.
pub fn judge(e: *const Entry, pages: []const page.Page, node_token: u64, vol_index: u32, taking_manager: bool, facts: Facts) Verdict {
    // Blind before anything else: every judgement below matches a page's node
    // token, so an entry under another token (or under none) has no verdict
    // (decision 0004 §4.4.2, KD19).
    if (e.node_token == 0 or e.node_token != node_token) return .unjudgeable;
    var verdict: Verdict = .joining;
    for (e.regionSlice()) |r| {
        const v = judgeRegion(e, r, pages, node_token, vol_index, taking_manager, facts) orelse continue;
        switch (v) {
            .stale => return .stale,
            .current => verdict = .current,
            .retired => if (verdict != .current) {
                verdict = .retired;
            },
            .joining => {},
            // A region of an entry whose token already matched is never
            // unjudgeable: that is the entry's own verdict, above.
            .unjudgeable => unreachable,
        }
    }
    return verdict;
}

fn judgeRegion(e: *const Entry, r: Region, pages: []const page.Page, node_token: u64, vol_index: u32, taking_manager: bool, facts: Facts) ?Verdict {
    switch (r.role) {
        // A manager of this host — registered (`p`) or durable (`m`) — is
        // superseded once a reservation stands while this host is
        // unregistered (decision 0004 §5.2; N1's frozen manager whose
        // appender-0 page still reads Live). With no reservation (a lapse)
        // appender 0's Live page decides.
        .pending, .manager => {
            if (r.slot != vol_index) return null;
            if (taking_manager or facts.reserved) return .stale;
            for (pages) |p| {
                if (p.appender_id == 0 and p.state == .live and p.identity.node_token == node_token and p.identity.writer_id == r.writer_id) return .current;
            }
            return .stale;
        },
        // A joined region names its page (review 4 blocker 1): current only
        // while that page is Live under its writer id and term, retired when
        // its own `r` covers that (appender id, term), else stale — a pair
        // released after its recovery and reused names another writer, and
        // the leave's own Free page clears the identity.
        .joiner, .leaving => if (r.joined) |j| {
            if (j.vol != vol_index) return null;
            for (pages) |p| {
                if (p.appender_id != j.appender_id) continue;
                if (p.state == .live and p.identity.node_token == node_token and p.identity.writer_id == r.writer_id and p.term == j.term) return .current;
                break;
            }
            return if (e.coversId(r.writer_id, j.appender_id, j.term, vol_index)) .retired else .stale;
        } else {
            // Not joined yet: the pages naming its writer id judge it.
            var named = false;
            var uncovered = false;
            for (pages) |p| {
                if (p.appender_id == 0 or p.identity.node_token != node_token or p.identity.writer_id != r.writer_id) continue;
                if (p.state == .live) return .current;
                named = true;
                if (!e.covers(p, vol_index)) uncovered = true;
            }
            if (uncovered) return .stale;
            return if (named) .retired else .joining;
        },
    }
}

// ---------------------------------------------------------------------------
// The rule.
// ---------------------------------------------------------------------------

/// Why a PR-capable mount was refused because the host's rule could not see
/// what it had to judge (decision 0004 §4.4.2, KD19). Every refusal names its
/// reason, counts it by name, and names the remedy.
pub const RefuseReason = enum {
    /// This daemon's host registry root is not the host's (its own `/run`,
    /// typically a container's), or pid 1's view has no such path.
    registry_root,
    /// The comparison itself failed: `/proc/1/root` under `hidepid` or an LSM,
    /// or any other errno. "Cannot compare" is never a pass.
    registry_root_unreadable,
    /// A live sibling under another node token in ANOTHER UTS namespace: two
    /// hostnames for one set on one host.
    node_token_hostname,
    /// A live sibling under another node token in THIS UTS namespace: the host's
    /// name changed while that sibling runs.
    node_token_renamed,
    /// A live sibling's entry carries no node token (an older binary).
    node_token_missing,

    /// The reason's name, as the log line and the counter carry it (KD19).
    pub fn text(self: RefuseReason) []const u8 {
        return switch (self) {
            .registry_root => "registry_root",
            .registry_root_unreadable => "registry_root_unreadable",
            .node_token_hostname => "node_token_hostname",
            .node_token_renamed => "node_token_renamed",
            .node_token_missing => "node_token_missing",
        };
    }

    /// This reason's counter. `pr_stale_sibling_refusals` counts every
    /// refusal, named or not (decision 0004 §9's counter row).
    pub fn count(self: RefuseReason) void {
        switch (self) {
            .registry_root => guard.stats.pr_stale_sibling_refusals_registry_root.inc(),
            .registry_root_unreadable => guard.stats.pr_stale_sibling_refusals_registry_root_unreadable.inc(),
            .node_token_hostname => guard.stats.pr_stale_sibling_refusals_node_token_hostname.inc(),
            .node_token_renamed => guard.stats.pr_stale_sibling_refusals_node_token_renamed.inc(),
            .node_token_missing => guard.stats.pr_stale_sibling_refusals_node_token_missing.inc(),
        }
    }
};

/// One blind-rule refusal: counted on the total and on its reason, logged by
/// name with its remedy (`why` says what was met). KD19's rows assert on the
/// reason name in this line.
fn refuseBlind(label: []const u8, path: []const u8, reason: RefuseReason, why: []const u8) void {
    guard.stats.pr_stale_sibling_refusals.inc();
    reason.count();
    log.err("{s} {s}: REFUSING the PR-capable mount: {s} (reason {s})", .{ label, path, why, reason.text() });
}

/// The reason a live sibling's node token cannot be judged, told apart by the
/// sibling's UTS namespace (decision 0004 §4.4.2): another namespace means
/// another hostname for this set on this host; this namespace means the host's
/// name changed while the sibling runs. A sibling whose namespace cannot be
/// read is reported under `node_token_hostname`, noting that.
fn tokenRefusalReason(e: *const Entry) RefuseReason {
    if (e.node_token == 0) return .node_token_missing;
    const mine = proc.utsNamespaceIno(0) orelse return .node_token_hostname;
    const theirs = proc.utsNamespaceIno(e.pid) orelse return .node_token_hostname;
    return if (theirs == mine) .node_token_renamed else .node_token_hostname;
}

/// The refusal's sentence, with the remedy KD19 names (the log line is the
/// operator's only instruction, so it says what to do).
fn tokenRefusalText(e: *const Entry, reason: RefuseReason, buf: []u8) []const u8 {
    const ns_unreadable = e.node_token != 0 and (proc.utsNamespaceIno(0) == null or proc.utsNamespaceIno(e.pid) == null);
    return switch (reason) {
        .node_token_hostname => std.fmt.bufPrint(buf, "a live sibling of the set (pid {d}) runs under another hostname (node token {x:0>16}{s}); it cannot be judged; refusing to register — one hostname per host for one set", .{ e.pid, e.node_token, if (ns_unreadable) ", its UTS namespace unreadable" else "" }) catch "a live sibling of the set runs under another hostname; it cannot be judged; refusing to register",
        .node_token_renamed => std.fmt.bufPrint(buf, "a live sibling of the set (pid {d}) runs under the host's previous name (node token {x:0>16}); the host's name changed while siblings run: restart them (or restore the name) before mounting again", .{ e.pid, e.node_token }) catch "a live sibling of the set runs under the host's previous name; restart it or restore the name",
        .node_token_missing => std.fmt.bufPrint(buf, "a live sibling's registry entry (pid {d}) carries no node token (an older binary); refusing to register — restart it on this binary", .{e.pid}) catch "a live sibling's registry entry carries no node token; refusing to register",
        .registry_root, .registry_root_unreadable => unreachable, // the root check writes its own line
    };
}

/// One unjudgeable live sibling, refused by name. `judge` reaches it too (the
/// re-judge, the beat); this is the entry-only path an admission runs.
fn refuseUnjudgeableEntry(ctx: RuleContext, e: *const Entry) error{SiblingUnjudgeable} {
    const reason = tokenRefusalReason(e);
    var wb: [400]u8 = undefined;
    refuseBlind(ctx.label, ctx.path, reason, tokenRefusalText(e, reason, &wb));
    return error.SiblingUnjudgeable;
}

/// Every live sibling's entry must be judgeable by this process's node token
/// (decision 0004 §4.4.2, KD19): an entry under another token, or under none,
/// makes the mount refuse rather than register beside a sibling its page
/// judgements cannot see.
///
/// It runs at EVERY admission, an adoption included, not only when the rule
/// reaches its kills: a mount that adopts a registration a foreign-token
/// sibling wrote is just as blind as one that writes its own, and row F16
/// measures the second mount refusing while the first lives. The caller holds
/// the registry's flock.
pub fn refuseUnjudgeableSiblings(ctx: RuleContext, reg: *const Registry) RuleError!void {
    const es = try reg.entries(ctx.gpa);
    defer ctx.gpa.free(es);
    for (es) |*e| {
        if (e.pid == ctx.local.pid) continue;
        if (e.node_token == ctx.node_token) continue;
        return refuseUnjudgeableEntry(ctx, e);
    }
}

pub const RuleContext = struct {
    /// The report's word for the judgement (set by `registerUnderRule`).
    facts: Facts = .{},
    gpa: Allocator,
    env: *const std.process.Environ.Map,
    /// The volume's block node (the log's name; its queue's in-flight
    /// writes are the drain's word).
    path: []const u8,
    local: claim.Identity,
    node_token: u64,
    vol_index: u32,
    taking_manager: bool,
    /// The namespace's kind in the log lines (a DATA namespace's admission
    /// runs the same rule, decision 0003 §DJ).
    label: []const u8 = "meta volume",
};

/// The registration lock's waiter bound: one NVMe admin command
/// (`nvme_core.admin_timeout`'s 30 s default). A holder keeps it for one
/// report, the page reads, one register or preempt, and the re-judge —
/// never a kill's wait (decision 0004 §4.4.2, DC20).
pub const REGISTRATION_LOCK_WAIT_MS: u64 = 30_000;

/// How long a SIGKILLed sibling may take to exit (an I/O it sleeps in must
/// complete first) before the registration is refused.
pub const KILL_WAIT_MS: i32 = 10_000;

/// One member-death bound: `T_owner`, the claim TTL. A frozen joiner is
/// killed only after the same holder has been named for this long
/// (decision 0004 §4.4.2).
pub const MEMBER_DEATH_BOUND_NS: u64 = claim.STALE_TTL_S * std.time.ns_per_s;

/// Which flock a manager's refusal waited on.
pub const LockFile = enum { registry, registration };

/// The same holder named by every refusal across one member-death bound.
pub const FrozenJoiner = struct {
    holder: ?marker.HolderId = null,
    since_ns: u64 = 0,

    /// Null, or a different holder, starts over. True when `holder` has
    /// been the named one for the whole bound.
    pub fn note(self: *FrozenJoiner, holder: ?marker.HolderId, now_ns: u64) bool {
        const h = holder orelse {
            self.* = .{};
            return false;
        };
        if (self.holder) |cur| {
            if (cur.pid != h.pid or cur.start_ticks != h.start_ticks) {
                self.* = .{ .holder = h, .since_ns = now_ns };
                return false;
            }
        } else {
            self.* = .{ .holder = h, .since_ns = now_ns };
            return false;
        }
        return now_ns -| self.since_ns >= MEMBER_DEATH_BOUND_NS;
    }

    pub fn clear(self: *FrozenJoiner) void {
        self.* = .{};
    }
};

/// How long a killed sibling's writes may take to leave the namespace's
/// block queue (a reconnecting controller requeues them) before the
/// registration is refused.
pub const DRAIN_WAIT_MS: u64 = 10_000;

pub const RuleError = RegistryError || Allocator.Error || error{ SiblingLive, SiblingUnjudgeable };

/// Under the registry's lock: every live sibling judged against `pages`; a
/// stale one proven dead or killed. Returns how many it killed.
/// `error.SiblingLive` = refused (logged by name, counted).
pub fn clearStaleSiblings(ctx: RuleContext, reg: *const Registry, pages: []const page.Page) RuleError!u32 {
    const es = try reg.entries(ctx.gpa);
    defer ctx.gpa.free(es);
    var killed: u32 = 0;
    for (es) |*e| {
        if (e.pid == ctx.local.pid) continue;
        const v = judge(e, pages, ctx.node_token, ctx.vol_index, ctx.taking_manager, ctx.facts);
        if (v == .unjudgeable) return refuseUnjudgeableEntry(ctx, e);
        if (v != .stale) {
            log.info("{s} {s}: sibling pid {d} of this set on this host is {s} — spared by the host's registration (its region is its own)", .{ ctx.label, ctx.path, e.pid, @tagName(v) });
            continue;
        }
        if (!proveIncarnationAlive(ctx, e)) {
            guard.stats.pr_siblings_proven_dead.inc();
            reg.remove(e.pid);
            continue;
        }
        if (killSibling(ctx, e)) |why| {
            guard.stats.pr_stale_sibling_refusals.inc();
            log.err("{s} {s}: REFUSING to register this host: the stale sibling pid {d} (writer id {x:0>16}) of this set on this host is alive and {s} — a registration would re-admit it to the device its region's recoverer owns; kill it by hand (`kill -9 {d}`) and remount", .{ ctx.label, ctx.path, e.pid, e.regionSlice()[0].writer_id, why, e.pid });
            return error.SiblingLive;
        }
        guard.stats.pr_stale_sibling_kills.inc();
        killed += 1;
        reg.remove(e.pid);
        log.warn("{s} {s}: killed the stale sibling pid {d} (writer id {x:0>16}) of this set on this host before registering the host — its region was recovered, a registration would have re-admitted its writes", .{ ctx.label, ctx.path, e.pid, e.regionSlice()[0].writer_id });
    }
    return killed;
}

/// Is any live sibling stale against `pages` (no kill)? An unjudgeable one
/// refuses: the re-judge cannot clear a registration beside it either.
fn anyStaleAlive(ctx: RuleContext, reg: *const Registry, pages: []const page.Page) RuleError!bool {
    const es = try reg.entries(ctx.gpa);
    defer ctx.gpa.free(es);
    for (es) |*e| {
        if (e.pid == ctx.local.pid) continue;
        const v = judge(e, pages, ctx.node_token, ctx.vol_index, ctx.taking_manager, ctx.facts);
        if (v == .unjudgeable) return refuseUnjudgeableEntry(ctx, e);
        if (v == .stale and proveIncarnationAlive(ctx, e)) return true;
    }
    return false;
}

/// The §DA proof (`proc.liveWriterPid`'s, for the pid the registry
/// names): the live process `e.pid` of this host IS one of the entry's
/// incarnations — the writer id recomputed from this host's boot id and
/// hostname, the slot, the pid and its start ticks.
fn proveIncarnationAlive(ctx: RuleContext, e: *const Entry) bool {
    for (e.regionSlice()) |r| {
        if (proc.isWriterIncarnation(ctx.local, e.pid, @truncate(r.slot), r.writer_id)) return true;
    }
    return false;
}

/// SIGKILL through a pidfd pinned by the entry's start ticks. Null = the
/// signal was sent or the process is already dead. The wait is `awaitDead`:
/// a mark-clause kill sends under the registration lock and waits outside
/// it (decision 0004 DC24).
fn signalPinned(env: *const std.process.Environ.Map, pid: u32, start_ticks: u64) ?[]const u8 {
    if (killRefusedSeam(env)) return "SQUEEZEFS_TEST_SIBLING_KILL_REFUSED denies its kill";
    const pinned = switch (proc.pin(@intCast(pid), start_ticks)) {
        .pinned => |p| p,
        .gone => return null,
        .refused => return "cannot be pinned (its pidfd_open or its start ticks' read refused)",
    };
    defer pinned.close();
    return switch (linux.errno(linux.pidfd_send_signal(pinned.fd, .KILL, null, 0))) {
        .SUCCESS, .SRCH => null,
        else => "refuses SIGKILL",
    };
}

fn signalSibling(ctx: RuleContext, e: *const Entry) ?[]const u8 {
    return signalPinned(ctx.env, e.pid, e.start_ticks);
}

/// Wait out a sibling already signalled. Null = dead.
fn awaitPinned(pid: u32, start_ticks: u64) ?[]const u8 {
    const pinned = switch (proc.pin(@intCast(pid), start_ticks)) {
        .pinned => |p| p,
        .gone => return null,
        .refused => return "cannot be pinned (its pidfd_open or its start ticks' read refused)",
    };
    defer pinned.close();
    var pfd = [1]linux.pollfd{.{ .fd = pinned.fd, .events = linux.POLL.IN, .revents = 0 }};
    while (true) {
        const pr_rc = linux.poll(&pfd, 1, KILL_WAIT_MS);
        switch (linux.errno(pr_rc)) {
            .SUCCESS => return if (pr_rc == 0) "did not exit within 10 s of SIGKILL (an I/O it sleeps in has not completed)" else null,
            .INTR => continue,
            else => return "cannot be awaited",
        }
    }
}

fn awaitDead(ctx: RuleContext, e: *const Entry) ?[]const u8 {
    _ = ctx;
    return awaitPinned(e.pid, e.start_ticks);
}

/// SIGKILL through a pidfd pinned by the entry's start ticks (`proc.pin`:
/// never another process under a recycled pid) and the whole thread
/// group's exit awaited on it. Null = dead; otherwise why not.
fn killSibling(ctx: RuleContext, e: *const Entry) ?[]const u8 {
    if (signalSibling(ctx, e)) |why| return why;
    return awaitDead(ctx, e);
}

pub const KillWord = enum { killed, not_joiner, still_alive };

/// The registry line is an unadmitted joiner of this set: joiner regions
/// only, none of them naming a page. A manager, a pending mount, a leaver,
/// or a joined page has a row. No line is not a joiner.
fn unadmittedJoiner(reg: *const Registry, h: marker.HolderId) bool {
    var buf: [4096]u8 = undefined;
    const text = reg.readEntry(h.pid, &buf) orelse return false;
    const e = Entry.parse(h.pid, text) orelse return false;
    if (e.start_ticks != h.start_ticks or e.n_regions == 0) return false;
    for (e.regionSlice()) |r| {
        if (r.role != .joiner or r.joined != null) return false;
    }
    return true;
}

/// Kill a frozen unadmitted joiner. No drain: it has written nothing. The
/// caller has already proved the freeze and the bound. `still_alive` retries
/// at the next beat (decision 0004 §4.4.2).
pub fn killFrozenJoiner(env: *const std.process.Environ.Map, reg: *const Registry, h: marker.HolderId) KillWord {
    if (!unadmittedJoiner(reg, h)) return .not_joiner;
    if (signalPinned(env, h.pid, h.start_ticks)) |why| {
        log.err("host fence: a frozen unadmitted joiner pid {d} {s} — the kill retries at the next beat (decision 0004 §4.4.2)", .{ h.pid, why });
        return .still_alive;
    }
    if (awaitPinned(h.pid, h.start_ticks)) |why| {
        log.err("host fence: a frozen unadmitted joiner pid {d} {s} — the kill retries at the next beat (decision 0004 §4.4.2)", .{ h.pid, why });
        return .still_alive;
    }
    marker.stats.host_fence_frozen_joiner_kills.inc();
    log.warn("host fence: killed a frozen unadmitted joiner pid {d} after one member-death bound (decision 0004 §4.4.2)", .{h.pid});
    return .killed;
}

/// A live manager entry other than this process, and whether its re-acquire
/// mark covers `since_ns` (decision 0004 DC24). `.unmarked` is the one to
/// signal: the re-read saw the namespace unreserved and it is reserved now,
/// and this manager did not acquire in between.
fn managerUnmarked(ctx: RuleContext, reg: *const Registry, since_ns: u64) (Allocator.Error || RegistryError)!union(enum) { marked, none, unmarked: Entry } {
    const es = try reg.entries(ctx.gpa);
    defer ctx.gpa.free(es);
    var unmarked: ?Entry = null;
    for (es) |e| {
        if (e.pid == ctx.local.pid) continue;
        var manages = false;
        for (e.regionSlice()) |r| if (r.role == .manager) {
            manages = true;
            break;
        };
        if (!manages) continue;
        if (!proc.incarnationAlive(e.pid, e.start_ticks)) continue;
        if (e.markedSince(since_ns)) return .marked;
        if (unmarked == null) unmarked = e;
    }
    if (unmarked) |e| return .{ .unmarked = e };
    return .none;
}

/// The namespace's block queue holds no write in flight (`/sys/class/block/
/// <node>/inflight`'s second word): a killed sibling's submitted writes —
/// a reconnecting controller's requeue included — have left it (review
/// major 9). Waited at most `DRAIN_WAIT_MS`; a node without the attribute
/// (not a block device) has nothing to drain.
fn awaitWriteDrain(ctx: RuleContext) error{SiblingLive}!void {
    // The node behind the path (a `/dev/disk/by-id` link resolves to it):
    // its `major:minor` names the queue (review round 3, major 3).
    var pb: [linux.PATH_MAX]u8 = undefined;
    const attr = blockAttrPath(&pb, ctx.path, "inflight") orelse return refuseDrain(ctx, "its block node's inflight attribute cannot be resolved");
    const deadline = clock.monotonicNs() +| DRAIN_WAIT_MS * std.time.ns_per_ms;
    var backoff_ns: u64 = std.time.ns_per_ms;
    while (true) {
        var tb: [64]u8 = undefined;
        const text = proc.readSmall(attr.ptr, &tb) orelse return refuseDrain(ctx, "its block node's inflight attribute is unreadable");
        var f = std.mem.tokenizeAny(u8, text, " \t\n");
        _ = f.next();
        const writes = std.fmt.parseInt(u64, f.next() orelse return refuseDrain(ctx, "its inflight attribute names no writes"), 10) catch return refuseDrain(ctx, "its inflight attribute is malformed"); // the attribute's writes word, decimal
        if (writes == 0) return;
        const now = clock.monotonicNs();
        if (now >= deadline) {
            guard.stats.pr_stale_sibling_refusals.inc();
            log.err("{s} {s}: REFUSING to register this host: {d} write(s) stay in flight on its block queue {d} ms after a stale sibling's kill — a registration would re-admit them", .{ ctx.label, ctx.path, writes, DRAIN_WAIT_MS });
            return error.SiblingLive;
        }
        const ts: linux.timespec = .{ .sec = 0, .nsec = @intCast(@min(backoff_ns, deadline - now)) };
        _ = linux.nanosleep(&ts, null);
        backoff_ns = @min(backoff_ns * 2, 100 * std.time.ns_per_ms); // a drain poll's ceiling: a requeue resolves in ≥ seconds
    }
}

fn refuseDrain(ctx: RuleContext, why: []const u8) error{SiblingLive} {
    guard.stats.pr_stale_sibling_refusals.inc();
    log.err("{s} {s}: REFUSING to register this host after a stale sibling's kill: {s} — its writes cannot be proven drained", .{ ctx.label, ctx.path, why });
    return error.SiblingLive;
}

/// `/sys/dev/block/<major>:<minor>/<attr>` of the block node `path` names
/// (links followed by `stat`); null = not a block node.
fn blockAttrPath(buf: *[linux.PATH_MAX]u8, path: []const u8, attr: []const u8) ?[:0]const u8 {
    var zb: [linux.PATH_MAX]u8 = undefined;
    const z = std.fmt.bufPrintSentinel(&zb, "{s}", .{path}, 0) catch return null;
    var st: linux.Statx = undefined;
    if (linux.errno(linux.statx(linux.AT.FDCWD, z.ptr, 0, .{ .TYPE = true }, &st)) != .SUCCESS) return null;
    if (!linux.S.ISBLK(st.mode)) return null;
    return std.fmt.bufPrintSentinel(buf, "/sys/dev/block/{d}:{d}/{s}", .{ st.rdev_major, st.rdev_minor, attr }, 0) catch null;
}

/// This process runs in the initial pid namespace: the registry's pids are
/// the kernel's own, so a sibling's entry is judged against its process
/// (reviews: major 8, round 3 major 6). `PROC_PID_INIT_INO`.
pub fn initialPidNamespace() bool {
    var st: linux.Statx = undefined;
    if (linux.errno(linux.statx(linux.AT.FDCWD, "/proc/self/ns/pid", 0, .{ .INO = true }, &st)) != .SUCCESS) return false;
    return st.ino == 0xEFFFFFFC; // PROC_PID_INIT_INO (include/linux/proc_ns.h)
}

/// Is this host registered under `c.key` (a registrant with our wire host
/// id and our key)? A registration there is idempotent: it re-admits nobody.
pub fn hostRegistered(c: *pr.Client) pr.Error!bool {
    return (try reportFacts(c)).registered;
}

pub const ReportFacts = struct { registered: bool, rtype: u8, facts: Facts };

/// One report: our host's registration and the reservation's word.
pub fn reportFacts(c: *pr.Client) pr.Error!ReportFacts {
    const me = try c.wireHostId();
    var rep = try c.report();
    defer rep.deinit(c.gpa);
    var registered = false;
    for (rep.registrants) |r| if (std.mem.eql(u8, &r.host_id, &me) and r.rkey == c.key) {
        registered = true;
    };
    return .{ .registered = registered, .rtype = rep.rtype, .facts = .{ .reserved = rep.rtype != 0 } };
}

/// One volume's appender pages, read under the registry's lock (the
/// judgement's input).
pub const PageSource = struct {
    ctx: *anyopaque,
    read: *const fn (ctx: *anyopaque, gpa: Allocator) error{ Io, Corrupt, OutOfMemory }![]page.Page,

    fn get(self: PageSource, gpa: Allocator) error{ OutOfMemory, PagesUnreadable }![]page.Page {
        return self.read(self.ctx, gpa) catch |e| switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            else => error.PagesUnreadable,
        };
    }
};

/// The register a caller runs under the rule (the gate's ladder, a
/// joiner's `fencing.joinAsRegistrant`).
pub const Register = struct {
    ctx: *anyopaque,
    run: *const fn (ctx: *anyopaque, c: *pr.Client) anyerror!void,
};

pub const AdmitError = RuleError || error{ PagesUnreadable, ReportUnreadable, RegisterFailed };

/// How many times a register is retried when a sibling turned stale
/// between the judgement and the register.
const REJUDGE_ROUNDS: u32 = 3; // a remote recovery's step 2 races at most once per death; three deaths at once is the bound

/// Under the registry's lock: the host registered under the key ⇒ false,
/// NOTHING mutated (review major 4: a register there could re-admit a host
/// a preempt removed after the report). Else the rule (stale siblings
/// proven dead or killed, their writes drained), the register, and the
/// pages judged AGAIN (review B2): a sibling a remote recovery turned stale
/// meanwhile takes the registration away (our key unregistered: the host
/// fenced again) before it is killed and the register retried. True =
/// registered afresh.
pub fn registerUnderRule(ctx_in: RuleContext, reg: *const Registry, c: *pr.Client, pages: PageSource, register: Register) AdmitError!bool {
    // Blindness is refused before anything else, an adoption included (an
    // entry under another node token makes every page judgement here blind).
    try refuseUnjudgeableSiblings(ctx_in, reg);
    // Every judgement reads the report afresh (the device's word on the
    // reservation, review round 3 major 1); one that fails judges nothing —
    // the registration is refused, no sibling killed on a guess.
    const first = reportFacts(c) catch return error.ReportUnreadable;
    if (first.registered) return false;
    var ctx = ctx_in;
    ctx.facts = first.facts;
    var round: u32 = 0;
    while (round < REJUDGE_ROUNDS) : (round += 1) {
        if (round > 0) ctx.facts = (reportFacts(c) catch return error.ReportUnreadable).facts;
        {
            const ps = try pages.get(ctx.gpa);
            defer ctx.gpa.free(ps);
            if (try clearStaleSiblings(ctx, reg, ps) > 0) try awaitWriteDrain(ctx);
        }
        // The kill and the drain stay outside. The re-read, the marker, the
        // register and the re-judge share the registration lock (decision
        // 0004 DC20). Data rules make no marker.
        var hold = reg.lockRegistration(0) catch return error.RegistryUnavailable;
        const preread = reportFacts(c) catch {
            hold.release();
            return error.ReportUnreadable;
        };
        if (preread.registered) {
            hold.release();
            return false;
        }
        // The re-judge's mark clause compares against this re-read
        // (decision 0004 DC24). A mark written earlier does not cover an
        // acquire that landed after it.
        const preread_ns = clock.boottimeNs();
        const was_unreserved = !preread.facts.reserved;
        ctx.facts = preread.facts;
        const mark = marker.createIn(reg.dir(), ctx.vol_index) catch {
            hold.release();
            return error.RegistryUnavailable;
        };
        register.run(register.ctx, c) catch {
            marker.dropSection(reg.dir(), &mark);
            hold.release();
            return error.RegisterFailed;
        };
        const judged = reportFacts(c) catch {
            // The register landed and the re-judge could not be read. The
            // withdrawal marker is published before the lock drops; the
            // unregister follows outside it.
            _ = marker.publishWithdrawal(reg.dir(), &mark);
            hold.release();
            _ = finishWithdrawal(reg, &mark, c);
            return error.ReportUnreadable;
        };
        ctx.facts = judged.facts;
        // Unreserved at the re-read, reserved now: either this host's
        // manager acquired and marked, or another host did. No mark since
        // the re-read kills the manager. The signal is under the lock; the
        // wait is outside (decision 0004 DC24). This process's own lapse
        // rule is not its own judge.
        var doomed: ?Entry = null;
        if (was_unreserved and judged.facts.reserved) {
            switch (managerUnmarked(ctx, reg, preread_ns) catch |e| {
                _ = marker.publishWithdrawal(reg.dir(), &mark);
                hold.release();
                _ = finishWithdrawal(reg, &mark, c);
                return e;
            }) {
                .marked, .none => {},
                .unmarked => |mgr| {
                    if (signalSibling(ctx, &mgr)) |why| {
                        _ = marker.publishWithdrawal(reg.dir(), &mark);
                        hold.release();
                        _ = finishWithdrawal(reg, &mark, c);
                        log.err("meta volume {s}: REFUSING the registration: the manager pid {d} {s} (decision 0004 DC24)", .{ ctx.path, mgr.pid, why });
                        return error.SiblingLive;
                    }
                    doomed = mgr;
                },
            }
        }
        const ps = pages.get(ctx.gpa) catch |e| {
            _ = marker.publishWithdrawal(reg.dir(), &mark);
            hold.release();
            _ = finishWithdrawal(reg, &mark, c);
            return e;
        };
        defer ctx.gpa.free(ps);
        const stale = doomed != null or (anyStaleAlive(ctx, reg, ps) catch |e| {
            _ = marker.publishWithdrawal(reg.dir(), &mark);
            hold.release();
            _ = finishWithdrawal(reg, &mark, c);
            return e;
        });
        if (!stale) {
            marker.dropSection(reg.dir(), &mark);
            hold.release();
            return true;
        }
        _ = marker.publishWithdrawal(reg.dir(), &mark);
        hold.release();
        if (doomed) |mgr| if (awaitDead(ctx, &mgr)) |why| {
            _ = finishWithdrawal(reg, &mark, c);
            log.err("meta volume {s}: REFUSING the registration: the manager pid {d} {s} (decision 0004 DC24)", .{ ctx.path, mgr.pid, why });
            return error.SiblingLive;
        };
        log.warn("meta volume {s}: a sibling of this set on this host turned stale between the rule and the register (a takeover or a remote recovery raced it) — the host's registration withdrawn before it is killed", .{ctx.path});
        if (!finishWithdrawal(reg, &mark, c)) {
            log.err("meta volume {s}: REFUSING the registration: it could not be withdrawn from a stale sibling's host", .{ctx.path});
            return error.SiblingLive;
        }
    }
    guard.stats.pr_stale_sibling_refusals.inc();
    log.err("meta volume {s}: REFUSING to register this host: siblings kept turning stale across {d} registers", .{ ctx.path, REJUDGE_ROUNDS });
    return error.SiblingLive;
}

/// Unregister outside the registration lock. Success removes the `.wd`.
/// Failure renames it `.abandoned` under the lock. True = unregistered.
fn finishWithdrawal(reg: *const Registry, mark: *const marker.Mark, c: *pr.Client) bool {
    c.unregister(c.key) catch {
        var hold = reg.lockRegistration(0) catch return false;
        defer hold.release();
        marker.abandonWithdrawal(reg.dir(), mark);
        return false;
    };
    marker.dropWithdrawal(reg.dir(), mark);
    return true;
}

pub const DataAdmitError = AdmitError || error{HostFenced};

/// A DATA namespace's registration under the rule (decision 0003 §DJ), the
/// caller holding the registry's lock: `data` registered under our wire host
/// id and key ⇒ false, nothing mutated (an adoption). Else, while this host is
/// registered on the METADATA namespace `meta` (a host a takeover or a death
/// preempt fenced never re-admits itself on data: `HostFenced`): the stale
/// siblings judged by `meta`'s report and the metadata pages (a data
/// namespace's WEAR report names every registrant a holder — it says nothing
/// of the manager role), killed, the DATA node's block queue drained
/// (`ctx.path`), the register, then the metadata registration and the pages
/// judged again — a sibling turned stale meanwhile takes the data
/// registration away before it is killed and the register retried. True =
/// registered afresh.
pub fn registerDataUnderRule(ctx_in: RuleContext, reg: *const Registry, meta: *pr.Client, data: *pr.Client, pages: PageSource, register: Register) DataAdmitError!bool {
    try refuseUnjudgeableSiblings(ctx_in, reg);
    const first = reportFacts(data) catch return error.ReportUnreadable;
    if (first.registered) return false;
    var ctx = ctx_in;
    var round: u32 = 0;
    while (round < REJUDGE_ROUNDS) : (round += 1) {
        const mf = reportFacts(meta) catch return error.ReportUnreadable;
        if (!mf.registered) return error.HostFenced;
        ctx.facts = mf.facts;
        {
            const ps = try pages.get(ctx.gpa);
            defer ctx.gpa.free(ps);
            if (try clearStaleSiblings(ctx, reg, ps) > 0) try awaitWriteDrain(ctx);
        }
        // Same section as the metadata rule (decision 0004 DC20). The data
        // fence's lock and the registry flock are already held (DC25).
        var hold = reg.lockRegistration(0) catch return error.RegistryUnavailable;
        errdefer hold.release();
        const preread = reportFacts(data) catch return error.ReportUnreadable;
        if (preread.registered) {
            hold.release();
            return false;
        }
        register.run(register.ctx, data) catch return error.RegisterFailed;
        // Registered: an error from here takes the registration back.
        errdefer data.unregister(data.key) catch {};
        const after = reportFacts(meta) catch return error.ReportUnreadable;
        if (!after.registered) return error.HostFenced;
        ctx.facts = after.facts;
        const ps = try pages.get(ctx.gpa);
        defer ctx.gpa.free(ps);
        const stale = try anyStaleAlive(ctx, reg, ps);
        hold.release();
        if (!stale) return true;
        log.warn("{s} {s}: a sibling of this set on this host turned stale between the rule and the register — the host's data registration withdrawn before it is killed", .{ ctx.label, ctx.path });
        data.unregister(data.key) catch {
            log.err("{s} {s}: REFUSING the registration: it could not be withdrawn from a stale sibling's host", .{ ctx.label, ctx.path });
            return error.SiblingLive;
        };
    }
    guard.stats.pr_stale_sibling_refusals.inc();
    log.err("{s} {s}: REFUSING to register this host: siblings kept turning stale across {d} registers", .{ ctx.label, ctx.path, REJUDGE_ROUNDS });
    return error.SiblingLive;
}

/// A co-located member's death (decision 0003 §DJ; the own-key guard: the
/// device cannot fence inside one host): the recoverer — on the same host —
/// takes the registry's lock and kills the process whose line names the dead
/// page `dead` on volume `ctx.vol_index` (its appender id and term, or no
/// page yet under its writer id) unless its own retirement covers that page,
/// proven dead by the §DA writer-id proof or SIGKILLed through a pidfd and
/// its exit awaited; then every data node's block queue drained. True =
/// proven (dead, killed, or no live process names the page); false = a kill
/// or a drain refused, or the page names another node — the recovery holds.
pub fn killColocatedMember(ctx_in: RuleContext, reg_in: Registry, dead: *const page.Page, data_nodes: []const []const u8) bool {
    var ctx = ctx_in;
    if (dead.identity.node_token != ctx.node_token) {
        log.err("recovery data fence: the dead appender {d} shares this host's registration key but names node 0x{x}, not this host's 0x{x} — two hosts under one host identifier (decision 0003 §DF.3): neither a device fence nor a kill holds it; the recovery holds", .{ dead.appender_id, dead.identity.node_token, ctx.node_token });
        return false;
    }
    var reg = reg_in;
    reg.lock() catch return false;
    defer reg.unlock();
    const es = reg.entries(ctx.gpa) catch return false;
    defer ctx.gpa.free(es);
    for (es) |*e| {
        if (e.pid == ctx.local.pid or !namesDeadPage(e, dead, ctx.vol_index)) continue;
        if (!proveIncarnationAlive(ctx, e)) {
            guard.stats.pr_siblings_proven_dead.inc();
            reg.remove(e.pid);
            continue;
        }
        if (killSibling(ctx, e)) |why| {
            guard.stats.pr_stale_sibling_refusals.inc();
            log.err("recovery data fence: the dead co-located member pid {d} (appender {d}) is alive and {s} — its writes could land in extents the recovery would reallocate; the recovery holds (kill it by hand: `kill -9 {d}`)", .{ e.pid, dead.appender_id, why, e.pid });
            return false;
        }
        guard.stats.pr_stale_sibling_kills.inc();
        reg.remove(e.pid);
        log.warn("recovery data fence: killed the dead co-located member pid {d} (appender {d}) — the device cannot fence inside this host", .{ e.pid, dead.appender_id });
    }
    // Every data node drained, killed or proven dead alike (review minor 2):
    // a gone process's writes can still sit in its block queues. The drain
    // reads the queue's in-flight count — a reconnect's requeued request is
    // idle and uncounted (§DF.13(g), the accepted residual).
    for (data_nodes) |n| {
        ctx.path = n;
        awaitWriteDrain(ctx) catch return false;
    }
    return true;
}

/// Does entry `e` name the dead page on volume `vol` (a joined line's page,
/// or an unjoined line under its writer id), uncovered by its retirement?
fn namesDeadPage(e: *const Entry, dead: *const page.Page, vol: u32) bool {
    for (e.regionSlice()) |r| {
        if ((r.role != .joiner and r.role != .leaving) or r.writer_id != dead.identity.writer_id) continue;
        if (r.joined) |j| {
            if (j.vol == vol and j.appender_id == dead.appender_id and j.term == dead.term and !e.coversId(r.writer_id, j.appender_id, j.term, vol)) return true;
        } else if (!e.covers(dead.*, vol)) return true;
    }
    return false;
}

/// A joiner's clean departure: does it take the host's registration with
/// it? Only when no other process of the set on this host lives AND the
/// reservation is not held under our key (review B3: a co-located manager
/// re-mounting holds it before it enrols as `m`; its `p` line counts here).
pub fn departUnregisters(reg: *const Registry, gpa: Allocator, c: *pr.Client, pid: u32) bool {
    const others = reg.othersAlive(gpa, pid) catch return false;
    if (others != 0) return false;
    const me = c.wireHostId() catch return false;
    var rep = c.report() catch return false;
    defer rep.deinit(c.gpa);
    return !rep.heldByHost(c.key, me);
}

// ---------------------------------------------------------------------------
// The host's registry root (decision 0004 §4.4.2, KD19).
// ---------------------------------------------------------------------------

/// A directory the comparison opened, or the errno that stopped it (`linux.E`
/// is an enum, not an error set: every failure here is a verdict, never a
/// propagated error).
const FdOrErr = union(enum) { fd: linux.fd_t, err: linux.E };

/// `path` as a NUL-terminated string for the syscall boundary (the registry
/// paths are short; `pathOf` and `entryPath` already treat a longer one as a
/// bug rather than a runtime case).
fn zpath(buf: *[linux.PATH_MAX]u8, path: []const u8) [:0]const u8 {
    return std.fmt.bufPrintSentinel(buf, "{s}", .{path}, 0) catch unreachable;
}

/// `openat2`'s `open_how`, the three u64s the 5.6 syscall takes. A larger
/// struct is a later kernel's; this size is the one every kernel since 5.6
/// accepts.
const OpenHow = extern struct {
    flags: u64,
    mode: u64,
    resolve: u64,

    comptime {
        if (@sizeOf(OpenHow) != 24) @compileError("open_how is three u64s");
    }
};

/// `linux/openat2.h`. `IN_ROOT` re-roots absolute symlinks and `..` inside the
/// directory fd. `NO_MAGICLINKS` refuses a procfs magic link with `ELOOP`
/// (decision 0004 §4.4.2, round-4 issue 2).
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_IN_ROOT: u64 = 0x10;

/// The host's view of `path`: `openat2(root, path, RESOLVE_IN_ROOT |
/// RESOLVE_NO_MAGICLINKS)`. A plain `openat` resolves an absolute symlink from
/// THIS process's root, so a host whose `/run` is an absolute symlink would
/// compare a private registry with itself. Measured on 7.3.1: a relative
/// symlink and `..` open, an absolute symlink stays inside `root`, and a magic
/// link (`/proc/self/root`) is `ELOOP`.
fn openInRoot(root: linux.fd_t, path: []const u8) FdOrErr {
    var buf: [linux.PATH_MAX]u8 = undefined;
    const how = OpenHow{
        .flags = @as(u32, @bitCast(linux.O{ .ACCMODE = .RDONLY, .DIRECTORY = true, .CLOEXEC = true })),
        .mode = 0,
        .resolve = RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS,
    };
    const rc = linux.syscall4(.openat2, @as(u32, @bitCast(root)), @intFromPtr(zpath(&buf, path).ptr), @intFromPtr(&how), @sizeOf(OpenHow));
    const e = linux.errno(rc);
    if (e != .SUCCESS) return .{ .err = e };
    return .{ .fd = @intCast(rc) };
}

/// Does `SQUEEZEFS_TEST_HOST_REGISTRY_DIR` move the host registries off the run
/// dir? Every process of such a rig shares the one directory it names, so the
/// root comparison would judge the rig's scratch, not the host: exempt (§4.4.2).
fn seamMovesRegistries(env: *const std.process.Environ.Map) bool {
    const raw = knobs.parseStr(env.get("SQUEEZEFS_TEST_HOST_REGISTRY_DIR")) orelse return false;
    return !std.mem.eql(u8, raw, "off");
}

/// The host's own root: `/proc/1/root`, or `SQUEEZEFS_TEST_PID1_ROOT` for the
/// units that build a two-view tree under scratch — it changes WHICH root is
/// compared, never whether the comparison runs.
fn hostRootName(env: *const std.process.Environ.Map) []const u8 {
    const raw = knobs.parseStr(env.get("SQUEEZEFS_TEST_PID1_ROOT")) orelse return "/proc/1/root";
    // The registry's default spelling: compare against pid 1, not a directory named "off".
    if (std.mem.eql(u8, raw, "off")) return "/proc/1/root";
    return raw;
}

fn openHostRoot(host_root: []const u8, buf: *[linux.PATH_MAX]u8) FdOrErr {
    const z = zpath(buf, host_root);
    // `O_PATH|O_DIRECTORY` (§4.4.2): the fd is only the root `openat2` walks
    // from. Resolving `/proc/1/root` is the ptrace-read `hidepid` or an LSM
    // takes away (EACCES, EPERM), which is this open's refusal.
    const rc = linux.open(z.ptr, .{ .PATH = true, .DIRECTORY = true, .CLOEXEC = true }, 0);
    const e = linux.errno(rc);
    if (e != .SUCCESS) return .{ .err = e };
    return .{ .fd = @intCast(rc) };
}

/// `(st_dev, st_ino)` of an open directory: what says two views are the same
/// directory (a zram or an LV's `dev` is its major:minor, which is what the
/// comparison needs).
fn devIno(fd: linux.fd_t) ?struct { dev: u64, ino: u64 } {
    var st: linux.Statx = undefined;
    if (linux.errno(linux.statx(fd, "", linux.AT.EMPTY_PATH, .{ .INO = true }, &st)) != .SUCCESS) return null;
    return .{ .dev = (@as(u64, st.dev_major) << 32) | st.dev_minor, .ino = st.ino };
}

/// KD19's mapping of a failed comparison, pinned by its own tie test: the
/// pid-1 side's `ENOENT`, with pid 1's root itself readable, is a private
/// `/run` the host never saw — the same shape as a mismatch, `registry_root`.
/// Every other failure says the host's view could not be read and is
/// `registry_root_unreadable`: `EACCES`/`EPERM` on `/proc/1/root` (procfs
/// `hidepid`, an LSM), `ELOOP` from `RESOLVE_NO_MAGICLINKS`, `EXDEV`, `ENOSYS`,
/// anything else.
pub fn rootRefusal(step: enum { host_root, walk }, err: linux.E) RefuseReason {
    if (step == .walk and err == .NOENT) return .registry_root;
    return .registry_root_unreadable;
}

pub const RootVerdict = union(enum) {
    /// The same directory: this daemon sees the siblings it must judge.
    same,
    /// Not the same, or not comparable: `reason` names the refusal, `errno`
    /// says how the comparison failed (`SUCCESS` for a plain mismatch).
    refused: struct { reason: RefuseReason, errno: linux.E },
};

/// This daemon's host registry root against the same path in the host's root
/// (§4.4.2). Every failure to compare refuses: "cannot compare" is never a
/// pass, because a rule that cannot see its siblings is the blindness KD19
/// turns into a loud refusal.
pub fn judgeRegistryRoot(env: *const std.process.Environ.Map, root: []const u8) RootVerdict {
    return judgeRootPaths(hostRootName(env), root);
}

/// The comparison itself, with the host's root named: a forked unit runs it
/// inside a namespace without touching an allocator.
pub fn judgeRootPaths(host_root: []const u8, root: []const u8) RootVerdict {
    var hbuf: [linux.PATH_MAX]u8 = undefined;
    const host: linux.fd_t = switch (openHostRoot(host_root, &hbuf)) {
        .fd => |f| f,
        .err => |e| return .{ .refused = .{ .reason = rootRefusal(.host_root, e), .errno = e } },
    };
    defer _ = linux.close(host);
    // The daemon's own view: a plain open of its absolute path, which resolves
    // in its own root — that is the whole point of the comparison.
    var obuf: [linux.PATH_MAX]u8 = undefined;
    const own_rc = linux.open(zpath(&obuf, root).ptr, .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .CLOEXEC = true }, 0);
    const own_errno = linux.errno(own_rc);
    if (own_errno != .SUCCESS) return .{ .refused = .{ .reason = rootRefusal(.walk, own_errno), .errno = own_errno } };
    const own: linux.fd_t = @intCast(own_rc);
    defer _ = linux.close(own);
    const theirs: linux.fd_t = switch (openInRoot(host, if (root.len > 0 and root[0] == '/') root[1..] else root)) {
        .fd => |f| f,
        .err => |e| return .{ .refused = .{ .reason = rootRefusal(.walk, e), .errno = e } },
    };
    defer _ = linux.close(theirs);
    const a = devIno(own) orelse return .{ .refused = .{ .reason = .registry_root_unreadable, .errno = .INVAL } };
    const b = devIno(theirs) orelse return .{ .refused = .{ .reason = .registry_root_unreadable, .errno = .INVAL } };
    if (a.dev == b.dev and a.ino == b.ino) return .same;
    return .{ .refused = .{ .reason = .registry_root, .errno = .SUCCESS } };
}

/// §4.4.2's registry-root comparison, refused, counted and logged by name with
/// its remedy. True = this daemon may run PR-capable mounts: its registry root
/// IS the host's (a private mount namespace that keeps `/run` passes, because
/// the registry is the same inode — which is why the check compares the root
/// and not the mount-namespace inode), or the registry seam exempts it.
pub fn hostRegistryRootAgrees(env: *const std.process.Environ.Map, root: []const u8, path: []const u8) bool {
    if (seamMovesRegistries(env)) return true;
    switch (judgeRegistryRoot(env, root)) {
        .same => return true,
        .refused => |r| {
            var wb: [640]u8 = undefined;
            const why: []const u8 = switch (r.reason) {
                .registry_root => std.fmt.bufPrint(&wb, "this daemon's host registry root {s} is not the host's ({s} answers {s}): the stale-sibling rule could not see its siblings and they could not see it; refusing PR-capable mounts: run the daemon with the host's /run", .{ root, hostRootName(env), if (r.errno == .NOENT) "no such path" else "another directory" }) catch return false,
                .registry_root_unreadable => std.fmt.bufPrint(&wb, "cannot read {s} (errno {d}) to compare the host registry root {s}; refusing PR-capable mounts: grant this daemon ptrace-read access to pid 1, or lift the hidepid/LSM restriction", .{ hostRootName(env), @backingInt(r.errno), root }) catch return false,
                else => unreachable,
            };
            refuseBlind("host registry", path, r.reason, why);
            return false;
        },
    }
}

/// What a PR-capable mount rides: its key (the host's, or the
/// incarnation's under the seam) and the set's registry (null under the
/// seam); `refused` = this daemon may not register here at all.
pub const Plan = struct {
    key: u64,
    reg: ?Registry,
    refused: bool = false,
};

/// The host's key and registry from the WIRE host id the namespace answers
/// (review major 7: a configured hostid's spelling, an explicit pair other
/// than the metadata controller's, never splits one device registrant into
/// two keys). No wire id (FID 0x81 refused or zero) ⇒ the incarnation's
/// key and no rule — logged.
pub fn plan(env: *const std.process.Environ.Map, c: *pr.Client, set_uuid: [16]u8, incarnation_key: u64, path: []const u8) Plan {
    if (perMountIdentity(env)) return .{ .key = incarnation_key, .reg = null };
    if (!initialPidNamespace()) {
        guard.stats.pr_stale_sibling_refusals.inc();
        log.err("meta volume {s}: REFUSING the PR-capable namespace: this daemon runs outside the initial pid namespace — the host's registry would name it by a pid its siblings cannot judge (pruned as dead, re-admitted unseen); run the daemon in the host's pid namespace", .{path});
        return .{ .key = incarnation_key, .reg = null, .refused = true };
    }
    const wire = c.wireHostId() catch {
        log.warn("meta volume {s}: the namespace answers no host identifier (FID 0x81) — the reservation key is this incarnation's and the stale-sibling rule cannot run", .{path});
        return .{ .key = incarnation_key, .reg = null };
    };
    const reg = Registry.runtime(env, .set, setRegistryKey(&wire, set_uuid));
    // The daemon creates its own registry root first, then compares it with the
    // host's (§4.4.2). A root that is not the host's leaves the rule blind to
    // every sibling on the host — a refusal, never a silent downgrade (KD19).
    var mkdirs = reg;
    mkdirs.mkdirs() catch {
        log.err("meta volume {s}: the host's registry of this set ({s}) cannot be created — refusing the PR-capable mount", .{ path, reg.dir() });
        return .{ .key = incarnation_key, .reg = null, .refused = true };
    };
    var root_buf: [linux.PATH_MAX]u8 = undefined;
    const root = zpath(&root_buf, journal_tail.HeldDir.runtimeOf(.hosts).slice());
    if (!hostRegistryRootAgrees(env, root, path)) return .{ .key = incarnation_key, .reg = null, .refused = true };
    return .{ .key = hostKey(&wire, set_uuid), .reg = reg };
}

fn ladderRegisterFn(ctx: *anyopaque, c: *pr.Client) anyerror!void {
    const path: *const []const u8 = @ptrCast(@alignCast(ctx));
    switch (try c.registerLadder()) {
        .registered => {},
        .recovered_own_stale => |n| log.warn("{s}: the register ladder unregistered {d} stale registration(s) of this host (another key under its wire id) before registering key {x:0>16}", .{ path.*, n, c.key }),
    }
}

/// A writer's PR posture and its host's registration, made BEFORE its
/// first device write (review round 3: a writer's open folds the replayed
/// ring and covers it through the namespace before its gate, and a host the
/// reservation does not admit is refused at the device — a successor from
/// another host could never take a manager's role over). Its `p` line is
/// written under the same lock (review B3). The gate adopts it.
pub const Early = struct {
    device: *pr_linux.Device,
    posture: pr.Posture,
    /// Null = no wire host id (the incarnation's key; the gate's ladder).
    reg: ?Registry,
    fresh: bool,
    /// This volume's writer id (its `p` line's word).
    writer_id: u64,
    /// How the hold was taken.
    hold: HoldOutcome = .acquired,
    /// The hold stays at this incarnation's end (`HoldOutcome.kept`).
    keep_hold: bool = false,
    /// A writer whose claim committed between the open's two probes, fenced
    /// by this open's preempt (the gate takes it over).
    fenced_writer: ?u64 = null,

    /// The hold under the early registration (WEAR on a symmetric volume,
    /// Write Exclusive on a flat one) — the caller read the claim right
    /// before it (review 5 major 1). A symmetric hold is a preempt-all
    /// (decision 0004 §4.4).
    pub fn takeHold(self: *Early, symmetric: bool, path: []const u8) error{Refused}!void {
        const c = self.posture.active;
        c.rtype = if (symmetric) nvme.RTYPE_WEAR else nvme.RTYPE_WRITE_EXCLUSIVE;
        self.hold = if (symmetric) try takeSuccession(c, path) else try holdReservation(c, path);
        self.keep_hold = self.hold.kept();
    }

    /// An open that failed before its gate adopted this: the `p` line out,
    /// the registration given back as `leaveRegistration` says (a hold
    /// taken by a preempt, or inherited, stays).
    pub fn discard(self: *Early, gpa: Allocator) void {
        const c = self.posture.active;
        if (self.reg) |*reg| {
            if (reg.lock()) |_| {
                defer reg.unlock();
                const me = proc.localIdentity();
                reg.removeRegion(me.pid, "p", self.writer_id, .any) catch {};
                if ((reg.othersAlive(gpa, me.pid) catch 1) == 0) leaveRegistration(c, self.keep_hold) catch {};
            } else |_| {}
        } else leaveRegistration(c, self.keep_hold) catch {};
        c.deinit();
        gpa.destroy(c);
        self.device.deinit();
        gpa.destroy(self);
    }
};

/// Null = no device fence here (a file, no char node, RESCAP 0, the seam):
/// the gate's own path. `error.Busy` = refused (logged by name). The
/// caller ran it only when the claim is not a fresh foreign one (review
/// round 3, major 2: `gate.probeClaim` first), and reads the claim again
/// before `Early.takeHold` (review 5 major 1: the rule here can outlast the
/// claim's TTL): the device's arbitration precedes every device write — two
/// successors' pre-gate writes cannot both land.
pub fn registerBeforeWrites(gpa: Allocator, env: *const std.process.Environ.Map, path: []const u8, set_uuid: [16]u8, vol_index: u32, local: claim.Identity, node_token: u64, pages: PageSource) error{ Busy, OutOfMemory }!?*Early {
    if (perMountIdentity(env)) return null;
    const dev = switch (pr_linux.resolve(gpa, path)) {
        .none => return null,
        .device => |d| d,
    };
    var keep_dev = false;
    defer if (!keep_dev) dev.deinit();
    const incarnation_key = claim.prKey(local.writerId(@intCast(vol_index)), local.boot_id);
    const c = try gpa.create(pr.Client);
    c.* = pr.Client.init(gpa, dev.face(), dev.nsid, incarnation_key) catch |e| {
        gpa.destroy(c);
        return e;
    };
    const posture = pr.resolvePosture(gpa, c, path);
    if (posture == .none) return null;
    var keep_client = false;
    defer if (!keep_client) {
        c.deinit();
        gpa.destroy(c);
    };
    const p = plan(env, c, set_uuid, incarnation_key, path);
    if (p.refused) return error.Busy;
    c.key = p.key;
    var fresh = true;
    if (p.reg) |reg_in| {
        var reg = reg_in;
        reg.lock() catch {
            log.err("meta volume {s}: the host's registry of this set ({s}) cannot be locked — refusing the mount (the stale-sibling rule cannot run)", .{ path, reg.dir() });
            return error.Busy;
        };
        defer reg.unlock();
        const ctx: RuleContext = .{ .gpa = gpa, .env = env, .path = path, .local = local, .node_token = node_token, .vol_index = vol_index, .taking_manager = false };
        var path_word = path;
        fresh = registerUnderRule(ctx, &reg, c, pages, .{ .ctx = @ptrCast(&path_word), .run = &ladderRegisterFn }) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            error.SiblingLive, error.SiblingUnjudgeable => return error.Busy,
            error.RegisterFailed => {
                log.err("meta volume {s}: this host's registration (key {x:0>16}) failed — refusing the mount", .{ path, c.key });
                return error.Busy;
            },
            error.RegistryUnavailable, error.PagesUnreadable, error.ReportUnreadable => {
                log.err("meta volume {s}: the stale-sibling rule could not read its inputs ({s}; registry {s}) — refusing to register this host", .{ path, @errorName(e), reg.dir() });
                return error.Busy;
            },
        };
        var lb: [64]u8 = undefined;
        reg.append(local.pid, local.start_ticks, node_token, regionLine(&lb, .{ .role = .pending, .slot = vol_index, .writer_id = local.writerId(@intCast(vol_index)) })) catch {
            log.err("meta volume {s}: this mount could not enter the host's registry of the set ({s}) — refusing the mount", .{ path, reg.dir() });
            if (fresh and departUnregisters(&reg, gpa, c, local.pid)) c.unregister(c.key) catch {};
            return error.Busy;
        };
    } else {
        var path_word = path;
        ladderRegisterFn(@ptrCast(&path_word), c) catch {
            log.err("meta volume {s}: this host's registration (key {x:0>16}) failed — refusing the mount", .{ path, c.key });
            return error.Busy;
        };
    }
    const e = gpa.create(Early) catch |err| {
        if (p.reg) |reg_in| {
            var reg = reg_in;
            if (reg.lock()) |_| {
                defer reg.unlock();
                reg.removeRegion(local.pid, "p", local.writerId(@intCast(vol_index)), .any) catch {};
                if ((reg.othersAlive(gpa, local.pid) catch 1) == 0) leaveRegistration(c, false) catch {};
            } else |_| {}
        } else leaveRegistration(c, false) catch {};
        return err;
    };
    e.* = .{ .device = dev, .posture = posture, .reg = p.reg, .fresh = fresh, .writer_id = local.writerId(@intCast(vol_index)) };
    keep_dev = true;
    keep_client = true;
    return e;
}

/// The report names THIS host the holder under our key (a failed report:
/// not proven).
pub fn holdStands(c: *pr.Client) bool {
    return c.admittedHere() catch false;
}

/// What a process leaving the host's registration gives back (the gate's
/// release, an open refused after its early registration), called when no
/// other process of the set on this host rides it. A non-holder
/// unregisters. A holder keeps the hold while another registrant stands (a
/// successor preempts it) or when `keep_hold` (it took the hold by a
/// preempt, inherited it, or preempted a member since: a release would
/// re-admit every host a preempt fenced — a superseded manager's checkpoint,
/// an open paused across the takeover, would land over acked metadata;
/// review 4 blocker 2); else the namespace's last registrant releases (zero
/// residue).
pub fn leaveRegistration(c: *pr.Client, keep_hold: bool) pr.Error!void {
    const me = try c.wireHostId();
    var rep = try c.report();
    defer rep.deinit(c.gpa);
    if (!rep.heldByHost(c.key, me)) return c.unregister(c.key);
    if (keep_hold) return;
    // Any registrant but this host's own keeps the hold.
    for (rep.registrants) |r| if (r.rkey != c.key or !std.mem.eql(u8, &r.host_id, &me)) return;
    return c.release();
}

pub const HoldOutcome = enum {
    /// Acquired on an unreserved namespace.
    acquired,
    /// Held under this key already: a predecessor's of this host, which may
    /// have taken it by a preempt.
    inherited,
    /// The holder's key preempted: its host fenced.
    preempted,

    /// The hold stays at this incarnation's last clean unmount.
    pub fn kept(o: HoldOutcome) bool {
        return o != .acquired;
    }
};

/// Other host ids named in `rep` (a preempt-all's victims, decision 0004
/// §4.4 step 6). The caller's own host id is not one.
fn otherHostIds(rep: nvme.Report, me: [16]u8) u64 {
    var n: u64 = 0;
    for (rep.registrants, 0..) |r, i| {
        if (std.mem.eql(u8, &r.host_id, &me)) continue;
        var earlier = false;
        for (rep.registrants[0..i]) |p| {
            if (std.mem.eql(u8, &p.host_id, &r.host_id)) {
                earlier = true;
                break;
            }
        }
        if (!earlier) n += 1;
    }
    return n;
}

fn registrantHost(rep: nvme.Report, key: u64, host: [16]u8) bool {
    for (rep.registrants) |r| if (r.rkey == key and std.mem.eql(u8, &r.host_id, &host)) return true;
    return false;
}

/// A symmetric volume's hold (decision 0004 §4.4 step 6). Report, acquire
/// only while the namespace is unreserved, then one preempt-all. The
/// namespace stays WEAR. `acquired` only when the report before the
/// preempt-all listed no other host and the generation moved by exactly
/// that command; anything else keeps the hold.
pub fn takeSuccession(c: *pr.Client, path: []const u8) error{Refused}!HoldOutcome {
    const me = c.wireHostId() catch {
        log.err("{s}: the host identifier could not be read before the preempt-all — refusing the mount", .{path});
        return error.Refused;
    };
    var r0 = c.report() catch {
        log.err("{s}: the reservation report failed before the preempt-all — refusing to arbitrate blind", .{path});
        return error.Refused;
    };
    defer r0.deinit(c.gpa);
    switch (r0.rtype) {
        0 => c.acquire() catch |e| switch (e) {
            error.ReservationConflict => {
                var again = c.report() catch {
                    log.err("{s}: the reservation report failed after an acquire conflict — refusing the mount", .{path});
                    return error.Refused;
                };
                defer again.deinit(c.gpa);
                if (again.rtype != nvme.RTYPE_WEAR) {
                    log.err("{s}: reservation acquire conflicted and the report's type is {d}, not WEAR — refusing the mount (decision 0004 §4.4)", .{ path, again.rtype });
                    return error.Refused;
                }
            },
            else => {
                log.err("{s}: reservation acquire failed on the PR-capable namespace — refusing the mount", .{path});
                return error.Refused;
            },
        },
        nvme.RTYPE_WEAR => if (!registrantHost(r0, c.key, me)) {
            log.err("{s}: this host is not registered under the standing WEAR reservation — a racing successor's preempt-all already removed it; refusing the mount (decision 0004 §4.4)", .{path});
            return error.Refused;
        },
        else => {
            log.err("{s}: metadata reservation type {d} is not WEAR — refusing the mount (decision 0004 §6.4)", .{ path, r0.rtype });
            return error.Refused;
        },
    }
    const others = otherHostIds(r0, me);
    const gen0 = r0.generation;
    c.preemptAllAndAbort() catch |e| switch (e) {
        error.ReservationConflict => {
            log.err("{s}: the preempt-all conflicted — a racing successor unregistered this host; refusing the mount (decision 0004 §4.4)", .{path});
            return error.Refused;
        },
        else => {
            log.err("{s}: the preempt-all failed — refusing the mount", .{path});
            return error.Refused;
        },
    };
    var r1 = c.report() catch {
        log.err("{s}: the reservation report failed after the preempt-all — refusing the mount", .{path});
        return error.Refused;
    };
    defer r1.deinit(c.gpa);
    if (r1.rtype != nvme.RTYPE_WEAR or !registrantHost(r1, c.key, me)) {
        log.err("{s}: the preempt-all did not leave this host registered under WEAR — refusing the mount", .{path});
        return error.Refused;
    }
    const gap = r1.generation -% gen0;
    const unseen: u64 = if (gap > 1) gap - 1 else 0;
    guard.stats.preempt_alls.inc();
    guard.stats.preempt_all_victims.add(others);
    guard.stats.preempt_all_unseen_events.add(unseen);
    if (others > 0) log.warn("{s}: preempt-all removed {d} other host id(s) under WEAR (decision 0004 §4.4)", .{ path, others });
    // Unreserved before, no other host, and the preempt-all was the only
    // generation bump (a plain acquire bumps nothing, P16).
    if (r0.rtype == 0 and others == 0 and gap == 1) return .acquired;
    return .preempted;
}

/// The hold under `c.rtype`: inherited (the report names our key), acquired,
/// or the report's ONE holder preempted (the superseded writer's host
/// unregistered, its writes refused from here). Symmetric volumes use
/// `takeSuccession` instead.
pub fn holdReservation(c: *pr.Client, path: []const u8) error{Refused}!HoldOutcome {
    const here = c.admittedHere() catch {
        log.err("{s}: the reservation report failed before the hold — refusing to arbitrate blind", .{path});
        return error.Refused;
    };
    if (here) return .inherited;
    c.acquire() catch |e| switch (e) {
        error.ReservationConflict => {
            const victim = (c.holder() catch {
                log.err("{s}: the reservation report failed after an acquire conflict — refusing to arbitrate blind", .{path});
                return error.Refused;
            }) orelse {
                log.err("{s}: reservation acquire conflicted but the report names no holder — refusing to arbitrate blind (single-writer guard)", .{path});
                return error.Refused;
            };
            // Our own key under another host id: a preempt of it would
            // unregister this host.
            if (victim == c.key) {
                log.err("{s}: the reservation is held under this host's key by another host id — refusing the hold (a preempt of our own key would unregister this host)", .{path});
                return error.Refused;
            }
            c.preempt(victim) catch {
                log.err("{s}: the preempt of the stale reservation holder key {x:0>16} failed — refusing the mount", .{ path, victim });
                return error.Refused;
            };
            guard.stats.preempts.inc();
            log.warn("{s}: preempted stale reservation holder key {x:0>16} (device-fenced takeover)", .{ path, victim });
            return .preempted;
        },
        else => {
            log.err("{s}: reservation acquire failed on the PR-capable namespace — refusing the mount", .{path});
            return error.Refused;
        },
    };
    return .acquired;
}

fn joinRegisterFn(ctx: *anyopaque, c: *pr.Client) anyerror!void {
    const env: *const std.process.Environ.Map = @ptrCast(@alignCast(ctx));
    _ = try fencing.joinAsRegistrant(c, fencing.declaredRegistrantCap(env));
}

/// A joiner's metadata registration (the join's, a lapse's re-registration).
pub fn joinRegister(env: *const std.process.Environ.Map) Register {
    return .{ .ctx = @constCast(env), .run = &joinRegisterFn };
}

/// A joined mount's registration on a PR-capable metadata namespace: the
/// char node, the client under the host's key, the set's registry.
pub const Joiner = struct {
    gpa: Allocator,
    device: *pr_linux.Device,
    client: *pr.Client,
    reg: Registry,
    slot: u32,
    writer_id: u64,
    /// The volume's lane index (a joined line names it).
    vol: u32,
    /// The page its `JoinAppender` answered (null before it).
    page: ?Joined = null,

    /// The admission: the host registered under the key ⇒ adopted (no
    /// device mutation); else the rule and the register
    /// (`registerUnderRule`). This process enters the registry under the
    /// same lock. Null = no device fence here (a file, a loop device, no
    /// char node, RESCAP 0, the seam, no wire host id): the join is the
    /// detection grade's.
    pub fn admit(ctx: RuleContext, set_uuid: [16]u8, slot: u32, writer_id: u64, pages: PageSource) error{ HostAdmissionRefused, OutOfMemory }!?*Joiner {
        if (perMountIdentity(ctx.env)) return null;
        const dev = switch (pr_linux.resolve(ctx.gpa, ctx.path)) {
            .none => return null,
            .device => |d| d,
        };
        errdefer dev.deinit();
        const c = try ctx.gpa.create(pr.Client);
        errdefer ctx.gpa.destroy(c);
        c.* = try pr.Client.init(ctx.gpa, dev.face(), dev.nsid, 0);
        errdefer c.deinit();
        const rescap = c.probeRescap() catch 0;
        if (rescap == 0) {
            c.deinit();
            ctx.gpa.destroy(c);
            dev.deinit();
            return null;
        }
        const p = plan(ctx.env, c, set_uuid, 0, ctx.path);
        if (p.refused) return error.HostAdmissionRefused;
        var reg = p.reg orelse {
            c.deinit();
            ctx.gpa.destroy(c);
            dev.deinit();
            return null;
        };
        c.key = p.key;
        reg.lock() catch {
            log.err("meta volume {s}: the host's registry of this set ({s}) cannot be locked — refusing the join (the stale-sibling rule cannot run)", .{ ctx.path, reg.dir() });
            return error.HostAdmissionRefused;
        };
        defer reg.unlock();
        // The unjoined line goes in before the section. A manager that finds
        // this process frozen in the register can see an unadmitted joiner
        // (no page) and kill it (decision 0004 §4.4.2). The rule skips this
        // process's own line.
        var lb: [64]u8 = undefined;
        reg.append(ctx.local.pid, ctx.local.start_ticks, ctx.node_token, regionLine(&lb, .{ .role = .joiner, .slot = slot, .writer_id = writer_id })) catch {
            log.err("meta volume {s}: this mount could not enter the host's registry of the set ({s}) — refusing the join", .{ ctx.path, reg.dir() });
            return error.HostAdmissionRefused;
        };
        const fresh = registerUnderRule(ctx, &reg, c, pages, .{ .ctx = @constCast(ctx.env), .run = &joinRegisterFn }) catch |e| {
            reg.removeRegion(ctx.local.pid, "j", writer_id, .unjoined) catch {};
            switch (e) {
                error.OutOfMemory => return error.OutOfMemory,
                error.SiblingLive, error.SiblingUnjudgeable => return error.HostAdmissionRefused,
                error.RegisterFailed => {
                    log.err("meta volume {s}: this host's registration (key {x:0>16}) failed — refusing the join", .{ ctx.path, c.key });
                    return error.HostAdmissionRefused;
                },
                error.RegistryUnavailable, error.PagesUnreadable, error.ReportUnreadable => {
                    log.err("meta volume {s}: the stale-sibling rule could not read its inputs ({s}) — refusing to register this host", .{ ctx.path, @errorName(e) });
                    return error.HostAdmissionRefused;
                },
            }
        };
        if (fresh) {
            guard.stats.pr_host_registrations.inc();
            log.info("meta volume {s}: registered this host on the namespace (key {x:0>16}) — every mount of the set on this host rides it", .{ ctx.path, c.key });
        } else {
            guard.stats.pr_host_adoptions.inc();
            log.info("meta volume {s}: this host is registered on the namespace under the set's key {x:0>16} — adopted (no device mutation)", .{ ctx.path, c.key });
        }
        const self = try ctx.gpa.create(Joiner);
        self.* = .{ .gpa = ctx.gpa, .device = dev, .client = c, .reg = reg, .slot = slot, .writer_id = writer_id, .vol = ctx.vol_index };
        return self;
    }

    /// The incarnation retired (its latch failed): declared with its page's
    /// appender id and term, so a sibling's registration spares this process
    /// while it re-joins — and only that page (review B1).
    pub fn declareRetired(self: *Joiner, appender_id: u32, term: u64) void {
        self.reg.lock() catch return;
        defer self.reg.unlock();
        var lb: [96]u8 = undefined;
        const me = proc.localIdentity();
        self.reg.append(me.pid, me.start_ticks, me.nodeToken(), retiredLine(&lb, .{ .writer_id = self.writer_id, .appender_id = appender_id, .term = term, .vol = self.vol })) catch {};
    }

    /// After its `JoinAppender`: the line names the page — its appender id
    /// and term on this volume (review 4 blocker 1: a pair released after a
    /// recovery and reused names another writer, so a line naming no page
    /// read such a member as joining). The joined line goes in BEFORE the
    /// unjoined one goes, and either failing fails the join (review 5 minor
    /// 2: never a process the rule cannot see).
    pub fn joined(self: *Joiner, appender_id: u32, term: u64) RegistryError!void {
        self.page = .{ .appender_id = appender_id, .term = term, .vol = self.vol };
        try self.reg.lock();
        defer self.reg.unlock();
        const me = proc.localIdentity();
        var lb: [96]u8 = undefined;
        try self.reg.append(me.pid, me.start_ticks, me.nodeToken(), regionLine(&lb, .{ .role = .joiner, .slot = self.slot, .writer_id = self.writer_id, .joined = self.page }));
        try self.reg.removeRegion(me.pid, "j", self.writer_id, .unjoined);
    }

    /// This incarnation's lines (a re-joined successor keeps the writer id;
    /// its other volumes' lines are their own).
    fn own(self: *const Joiner) Registry.Which {
        return if (self.page) |p| .{ .page = .{ .aid = p.appender_id, .vol = p.vol } } else .unjoined;
    }

    /// Before the clean leave: this region marked leaving (`l`) — judged by
    /// its page while the leave's writes are still to land (review round 3,
    /// major 4).
    pub fn depart(self: *Joiner) void {
        self.reg.lock() catch return;
        defer self.reg.unlock();
        const me = proc.localIdentity();
        var lb: [96]u8 = undefined;
        self.reg.append(me.pid, me.start_ticks, me.nodeToken(), regionLine(&lb, .{ .role = .leaving, .slot = self.slot, .writer_id = self.writer_id, .joined = self.page })) catch return;
        self.reg.removeRegion(me.pid, "j", self.writer_id, self.own()) catch {};
    }

    /// After the leave's last write: this region out of the registry.
    pub fn departed(self: *Joiner) void {
        self.reg.lock() catch return;
        defer self.reg.unlock();
        self.reg.removeRegion(proc.localIdentity().pid, "jlr", self.writer_id, self.own()) catch {};
    }

    /// The last process of the set on this host takes the host's
    /// registration with it when no reservation stands under the key (zero
    /// residue); otherwise it stays the siblings'.
    pub fn deinit(self: *Joiner) void {
        if (self.reg.lock()) |_| {
            defer self.reg.unlock();
            const pid = proc.localIdentity().pid;
            self.reg.removeRegion(pid, "jlr", self.writer_id, self.own()) catch {};
            if (departUnregisters(&self.reg, self.gpa, self.client, pid)) self.client.unregister(self.client.key) catch {};
        } else |_| {}
        self.client.deinit();
        self.gpa.destroy(self.client);
        self.device.deinit();
        self.gpa.destroy(self);
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const fuzzkit = @import("../../core/fuzzkit.zig");
const fake = @import("fake_nvme.zig");

fn entryFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [512]u8 = undefined;
    const len = smith.slice(&buf);
    _ = Entry.parse(7, buf[0..len]);
}

test "fuzz: the host registry's entry decoder Entry.parse is total on arbitrary text" {
    try fuzzkit.fuzz({}, entryFuzz, .{ .corpus = &.{ fuzzkit.slice("77\nj 9 00000000000000b1\nr 00000000000000b1\n"), fuzzkit.slice("x\nm 0 zz\nj\n") } });
}

fn pg(id: u32, state: page.State, node: u64, wid: u64) page.Page {
    return pgTerm(id, state, node, wid, 0);
}

fn pgTerm(id: u32, state: page.State, node: u64, wid: u64, term: u64) page.Page {
    var p = page.Page.free(id, 1, .{});
    p.state = state;
    p.term = term;
    p.identity = .{ .node_token = node, .mount_slot = 7, .writer_id = wid };
    return p;
}

test "the host key and the registry keys: one per (host identity, set), stable, never 0; another identity or another set differs" {
    const set_a: [16]u8 = @splat(1);
    const set_b: [16]u8 = @splat(2);
    try testing.expectEqual(hostKey("aaaa", set_a), hostKey("aaaa", set_a));
    try testing.expect(hostKey("aaaa", set_a) != hostKey("bbbb", set_a));
    try testing.expect(hostKey("aaaa", set_a) != hostKey("aaaa", set_b));
    try testing.expect(hostKey("aaaa", set_a) != 0);
    try testing.expect(setRegistryKey("aaaa", set_a) != hostKey("aaaa", set_a));
    try testing.expect(fabricRegistryKey("nqn.s", "nqn.h", "id") != fabricRegistryKey("nqn.s", "nqn.h", "id2"));
}

test "a joiner's claimed page key against the joined volume's report (decision 0004 §6.1, KD18): ok only when a registrant holds it under the host id it derives from (hostKey(registrant's wire host id, set)); a key held by no registrant (stale, another set's) is absent; one held only under host ids it does not derive from is a mismatch; under incarnation keys (the per-mount seam) any registrant holding it suffices — and another live host's key passes: the check binds the key to SOME registered host, not to the joiner's (decision 0003 §DO.28's residue)" {
    const set: [16]u8 = @splat(7);
    const host_a: [16]u8 = "host-a-wire-id01".*;
    const host_b: [16]u8 = "host-b-wire-id02".*;
    const fencer_b: [16]u8 = "fencer-b-wire-03".*;
    const key_a = hostKey(&host_a, set);
    const key_b = hostKey(&host_b, set);
    var regs = [_]nvme.Registrant{
        .{ .cntlid = 1, .holds = true, .rkey = key_b, .host_id = host_b },
        .{ .cntlid = 2, .holds = false, .rkey = key_b, .host_id = fencer_b },
        .{ .cntlid = 3, .holds = false, .rkey = key_a, .host_id = host_a },
    };
    const rep: nvme.Report = .{ .generation = 4, .rtype = nvme.RTYPE_WERO, .regctl = regs.len, .ptpls = 0, .extended = true, .registrants = &regs };
    try testing.expectEqual(JoinKeyVerdict.ok, joinKeyVerdict(rep, key_a, set, true));
    // Host B's key: right from host B's co-located joiner — and the same
    // answer to a joiner on host A claiming it (the residue: the report
    // names no requester).
    try testing.expectEqual(JoinKeyVerdict.ok, joinKeyVerdict(rep, key_b, set, true));
    try testing.expectEqual(JoinKeyVerdict.absent, joinKeyVerdict(rep, key_a ^ 1, set, true));
    try testing.expectEqual(JoinKeyVerdict.absent, joinKeyVerdict(rep, 0, set, true));
    // Host A claiming another set's key for itself: no registrant derives it.
    try testing.expectEqual(JoinKeyVerdict.absent, joinKeyVerdict(rep, hostKey(&host_a, @splat(8)), set, true));
    // Registered under a key its host id does not derive (an incarnation
    // key on a host-keyed set): a mismatch; under incarnation keys, ok.
    regs[2].rkey = 0x1234;
    try testing.expectEqual(JoinKeyVerdict.mismatch, joinKeyVerdict(rep, 0x1234, set, true));
    try testing.expectEqual(JoinKeyVerdict.ok, joinKeyVerdict(rep, 0x1234, set, false));
    try testing.expectEqual(JoinKeyVerdict.absent, joinKeyVerdict(rep, 0x9999, set, false));
}

test "the judgement: a Live page names a current sibling; a Recovering, Recovered or Free page under its writer id is stale unless its own retirement (writer id, appender id, term) covers it; no page yet is joining; the manager is current only on appender 0's Live page, stale once the role passed or while this mount takes it over; a manager of this host (pending or durable) is stale while a reservation stands and this host is unregistered; another node's page never names a sibling" {
    const node: u64 = 0xA;
    var e = Entry.parse(42, "77 000000000000000a\nj 9 00000000000000b1\n").?;
    try testing.expectEqual(Verdict.joining, judge(&e, &.{}, node, 0, false, .{}));
    try testing.expectEqual(Verdict.current, judge(&e, &.{pg(3, .live, node, 0xb1)}, node, 0, false, .{}));
    try testing.expectEqual(Verdict.stale, judge(&e, &.{pg(3, .recovered, node, 0xb1)}, node, 0, false, .{}));
    try testing.expectEqual(Verdict.stale, judge(&e, &.{pg(3, .recovering, node, 0xb1)}, node, 0, false, .{}));
    try testing.expectEqual(Verdict.stale, judge(&e, &.{pg(3, .free, node, 0xb1)}, node, 0, false, .{}));
    // A re-join in place: the old page Recovered, the new one Live.
    try testing.expectEqual(Verdict.current, judge(&e, &.{ pg(3, .recovered, node, 0xb1), pg(5, .live, node, 0xb1) }, node, 0, false, .{}));
    // Another node's page under the same writer id is not this sibling's.
    try testing.expectEqual(Verdict.joining, judge(&e, &.{pg(3, .recovered, 0xB, 0xb1)}, node, 0, false, .{}));
    var r = Entry.parse(43, "78 000000000000000a\nj 9 00000000000000b2\nr 00000000000000b2 4 0 0\n").?;
    try testing.expectEqual(Verdict.retired, judge(&r, &.{pg(4, .recovered, node, 0xb2)}, node, 0, false, .{}));
    // An `r` line short of a word retires nothing.
    var bare = Entry.parse(43, "78 000000000000000a\nj 9 00000000000000b2\nr 00000000000000b2 4\n").?;
    try testing.expectEqual(Verdict.stale, judge(&bare, &.{pg(4, .recovered, node, 0xb2)}, node, 0, false, .{}));
    // A manager of this host — pending or durable — is stale while a
    // reservation stands (this host is unregistered: not admitted, whatever
    // the holder pointer names). With no reservation appender 0's Live page
    // decides.
    var p = Entry.parse(45, "80 000000000000000a\np 0 00000000000000c9\n").?;
    try testing.expectEqual(Verdict.stale, judge(&p, &.{pg(0, .live, node, 0xc9)}, node, 0, false, .{ .reserved = true }));
    try testing.expectEqual(Verdict.stale, judge(&p, &.{pg(0, .live, node, 0xd1)}, node, 0, false, .{}));
    try testing.expectEqual(Verdict.current, judge(&p, &.{pg(0, .live, node, 0xc9)}, node, 0, false, .{}));
    try testing.expectEqual(Verdict.joining, judge(&p, &.{pg(0, .live, node, 0xd1)}, node, 1, false, .{ .reserved = true }));
    var mm = Entry.parse(47, "82 000000000000000a\nm 0 00000000000000ca\n").?;
    try testing.expectEqual(Verdict.stale, judge(&mm, &.{pg(0, .live, node, 0xca)}, node, 0, false, .{ .reserved = true }));
    var m = Entry.parse(44, "79 000000000000000a\nm 0 00000000000000c1\nm 1 00000000000000c2\n").?;
    try testing.expectEqual(Verdict.current, judge(&m, &.{pg(0, .live, node, 0xc1)}, node, 0, false, .{}));
    try testing.expectEqual(Verdict.stale, judge(&m, &.{pg(0, .live, node, 0xd1)}, node, 0, false, .{}));
    try testing.expectEqual(Verdict.stale, judge(&m, &.{pg(0, .live, node, 0xc1)}, node, 0, true, .{}));
    // Volume 1: only the manager region at slot 1 is judged.
    try testing.expectEqual(Verdict.current, judge(&m, &.{pg(0, .live, node, 0xc2)}, node, 1, false, .{}));
    try testing.expect(Entry.parse(1, "") == null);
    try testing.expect(Entry.parse(1, "x\n") == null);
}

test "re-acquire mark: the line records the acquire's boottime and generation, a later mark replaces it, and a mark from before the re-read does not cover it (decision 0004 DC24)" {
    var tb: [128]u8 = undefined;
    const base = try std.fmt.bufPrint(&tb, "/build/tmp/host-fence-mark-{d}", .{linux.getpid()});
    defer Io.Dir.cwd().deleteTree(testing.io, base) catch {};
    var reg = Registry.at(base, .set, 0xdc24);
    reg.lock() catch return @import("../../core/testkit.zig").skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    defer reg.unlock();
    const me = proc.localIdentity();
    var lb: [64]u8 = undefined;
    try reg.append(me.pid, me.start_ticks, 0xA, regionLine(&lb, .{ .role = .manager, .slot = 0, .writer_id = 0xabc }));
    try reg.noteReacquire(me.pid, me.start_ticks, 0xA, 1000, 4);
    try reg.noteReacquire(me.pid, me.start_ticks, 0xA, 2500, 9);
    var rb: [4096]u8 = undefined;
    const text = reg.readEntry(me.pid, &rb) orelse return error.Missing;
    const e = Entry.parse(me.pid, text) orelse return error.Parse;
    try testing.expectEqual(@as(u64, 2500), e.reacquire_ns);
    try testing.expectEqual(@as(u32, 9), e.reacquire_gen);
    try testing.expect(e.markedSince(2500));
    try testing.expect(e.markedSince(1000));
    try testing.expect(!e.markedSince(2501));
    try testing.expect(!Entry.parse(1, "1 000000000000000a\n").?.markedSince(1));
    // The manager line survived the mark.
    try testing.expectEqual(@as(usize, 1), e.n_regions);
    try testing.expectEqual(Role.manager, e.regionSlice()[0].role);
}

test "frozen joiner: the bound is one member-death, T_owner, and a different holder or an unnamed refusal resets it" {
    try testing.expectEqual(claim.STALE_TTL_S * std.time.ns_per_s, MEMBER_DEATH_BOUND_NS);
    try testing.expectEqual(@import("lease_clock.zig").T_OWNER_MS * std.time.ns_per_ms, MEMBER_DEATH_BOUND_NS);
    var w: FrozenJoiner = .{};
    const a: marker.HolderId = .{ .pid = 3, .start_ticks = 9 };
    try testing.expect(!w.note(a, 0));
    try testing.expect(!w.note(a, MEMBER_DEATH_BOUND_NS - 1));
    try testing.expect(w.note(a, MEMBER_DEATH_BOUND_NS));
    const b: marker.HolderId = .{ .pid = 4, .start_ticks = 9 };
    try testing.expect(!w.note(b, MEMBER_DEATH_BOUND_NS));
    try testing.expect(!w.note(null, MEMBER_DEATH_BOUND_NS + 1));
    try testing.expect(w.holder == null);
}

const Paused = struct {
    pid: linux.pid_t,
    hold: linux.fd_t,

    fn start() !Paused {
        var fds: [2]linux.fd_t = undefined;
        if (linux.errno(linux.pipe2(&fds, .{ .CLOEXEC = true })) != .SUCCESS) return error.Pipe;
        const rc = linux.fork();
        if (linux.errno(rc) != .SUCCESS) return error.Fork;
        if (rc == 0) {
            _ = linux.close(fds[1]);
            var byte: u8 = 0;
            _ = linux.read(fds[0], @ptrCast(&byte), 1);
            linux.exit_group(0);
        }
        _ = linux.close(fds[0]);
        return .{ .pid = @intCast(rc), .hold = fds[1] };
    }

    fn stop(self: Paused) !void {
        try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.kill(self.pid, .STOP)));
        const id: u32 = @intCast(self.pid);
        while (proc.ruleState(id) != .frozen) std.Thread.yield() catch {};
    }

    fn deinit(self: *Paused) void {
        _ = linux.close(self.hold);
        _ = linux.kill(self.pid, .KILL);
        var status: i32 = 0;
        _ = linux.waitpid(self.pid, &status, 0);
    }
};

test "frozen joiner: a stopped unadmitted joiner is killed, a stopped manager is not, and a live marker names its holder" {
    var tb: [128]u8 = undefined;
    const base = try std.fmt.bufPrint(&tb, "/build/tmp/host-fence-frozen-{d}", .{linux.getpid()});
    defer Io.Dir.cwd().deleteTree(testing.io, base) catch {};
    var reg = Registry.at(base, .set, 0xdc21);
    reg.lock() catch return @import("../../core/testkit.zig").skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    defer reg.unlock();
    const mark = marker.createIn(reg.dir(), 0) catch return error.Marker;
    const named = marker.liveHolder(reg.dir()) orelse return error.Holder;
    try testing.expectEqual(proc.localIdentity().pid, named.pid);
    marker.dropSection(reg.dir(), &mark);
    try testing.expect(marker.liveHolder(reg.dir()) == null);

    var joiner = Paused.start() catch return error.Fork;
    defer joiner.deinit();
    var manager = Paused.start() catch return error.Fork;
    defer manager.deinit();
    const jid: u32 = @intCast(joiner.pid);
    const mid: u32 = @intCast(manager.pid);
    const jticks = switch (proc.startTicksAnyState(jid)) {
        .ticks => |t| t,
        else => return error.Ticks,
    };
    const mticks = switch (proc.startTicksAnyState(mid)) {
        .ticks => |t| t,
        else => return error.Ticks,
    };
    try joiner.stop();
    try manager.stop();
    var lb: [64]u8 = undefined;
    try reg.append(jid, jticks, 0xA, regionLine(&lb, .{ .role = .joiner, .slot = 1, .writer_id = 0x11 }));
    try reg.append(mid, mticks, 0xA, regionLine(&lb, .{ .role = .manager, .slot = 0, .writer_id = 0x22 }));
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    const before = marker.stats.host_fence_frozen_joiner_kills.load();
    try testing.expectEqual(KillWord.not_joiner, killFrozenJoiner(&env, &reg, .{ .pid = mid, .start_ticks = mticks }));
    try testing.expectEqual(proc.RuleState.frozen, proc.ruleState(mid));
    try testing.expectEqual(KillWord.killed, killFrozenJoiner(&env, &reg, .{ .pid = jid, .start_ticks = jticks }));
    try testing.expectEqual(before + 1, marker.stats.host_fence_frozen_joiner_kills.load());
    // A joined page is a row. The kill does not signal it.
    try reg.removeRegion(mid, "m", 0x22, .any);
    try reg.append(mid, mticks, 0xA, regionLine(&lb, .{ .role = .joiner, .slot = 1, .writer_id = 0x33, .joined = .{ .appender_id = 2, .term = 1, .vol = 0 } }));
    try testing.expectEqual(KillWord.not_joiner, killFrozenJoiner(&env, &reg, .{ .pid = mid, .start_ticks = mticks }));
    try testing.expectEqual(proc.RuleState.frozen, proc.ruleState(mid));
}

test "registration lock: two open file descriptions of .register exclude each other, and the registry flock is a different file (decision 0004 DC20)" {
    var tb: [128]u8 = undefined;
    const base = try std.fmt.bufPrint(&tb, "/build/tmp/host-fence-reglock-{d}", .{linux.getpid()});
    defer Io.Dir.cwd().deleteTree(testing.io, base) catch {};
    var reg = Registry.at(base, .set, 0xdc20);
    reg.lock() catch return @import("../../core/testkit.zig").skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    defer reg.unlock();
    var a = try reg.lockRegistration(0);
    defer a.release();
    // The flock and the registration lock are held together: that is the
    // rule's order, not an inversion.
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.RegistryUnavailable, reg.lockRegistration(1));
    try log.testing.expectAllFired();
    a.release();
    var b = try reg.lockRegistration(50);
    b.release();
    // Flock under the registration lock is the inversion.
    var c = try reg.lockRegistration(50);
    defer c.release();
    var other = Registry.at(base, .set, 0xdc21);
    log.testing.expectErrorLogs(1);
    try other.lock();
    other.unlock();
    try log.testing.expectAllFired();
}

test "the registry's fabric users per namespace (review minor 2 of decision 0003 §DG): one process standing fencers for two namespaces of one subsystem holds a line per namespace — the first namespace's give-back leaves the process a user of the association, the second's ends it" {
    var tb: [128]u8 = undefined;
    const base = try std.fmt.bufPrint(&tb, "/build/tmp/host-fence-users-{d}", .{linux.getpid()});
    defer Io.Dir.cwd().deleteTree(testing.io, base) catch {};
    var reg = Registry.at(base, .fabric, 0x77);
    reg.lock() catch return @import("../../core/testkit.zig").skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    defer reg.unlock();
    const me = proc.localIdentity();
    try reg.append(me.pid, me.start_ticks, 0xA, "f 1\n");
    try reg.append(me.pid, me.start_ticks, 0xA, "f 2\n");
    try testing.expect(try reg.removeLine(me.pid, "f 1\n"));
    {
        const es = try reg.entries(testing.allocator);
        defer testing.allocator.free(es);
        try testing.expectEqual(@as(usize, 1), es.len);
    }
    try testing.expect(!(try reg.removeLine(me.pid, "f 2\n")));
    const es = try reg.entries(testing.allocator);
    defer testing.allocator.free(es);
    try testing.expectEqual(@as(usize, 0), es.len);
    try testing.expect(!(try reg.removeLine(me.pid, "f 2\n")));
    // Another live process's line for a namespace counts; its other lines do
    // not, a dead one's never (the parent of this process stands in).
    const parent: u32 = @intCast(linux.getppid());
    const parent_ticks = proc.startTicksOf(parent) orelse return;
    try reg.append(parent, parent_ticks, 0xA, "f 2\n");
    try testing.expectEqual(@as(usize, 1), try reg.othersWithLine(testing.allocator, me.pid, "f 2\n"));
    try testing.expectEqual(@as(usize, 0), try reg.othersWithLine(testing.allocator, me.pid, "f 1\n"));
    try testing.expectEqual(@as(usize, 0), try reg.othersWithLine(testing.allocator, parent, "f 2\n"));
    reg.remove(parent);
}

test "the registry: entries append and parse; a dead process's entry is pruned by its start ticks; the flock serializes; a region's lines leave alone; a reused pid's stale file is replaced" {
    var tb: [128]u8 = undefined;
    const base = try std.fmt.bufPrint(&tb, "/build/tmp/host-fence-unit-{d}", .{linux.getpid()});
    defer Io.Dir.cwd().deleteTree(testing.io, base) catch {};
    var reg = Registry.at(base, .set, 0x1234);
    reg.lock() catch return @import("../../core/testkit.zig").skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    defer reg.unlock();
    const me = proc.localIdentity();
    var lb: [64]u8 = undefined;
    try reg.append(me.pid, me.start_ticks, 0xA, regionLine(&lb, .{ .role = .joiner, .slot = 9, .writer_id = 0xb1 }));
    try reg.append(me.pid, me.start_ticks, 0xA, retiredLine(&lb, .{ .writer_id = 0xb1, .appender_id = 3, .term = 2, .vol = 0 }));
    try reg.append(me.pid, me.start_ticks, 0xA, regionLine(&lb, .{ .role = .manager, .slot = 1, .writer_id = 0xc2 }));
    // A dead process (pid 1's start ticks never match 1).
    try reg.append(1, 1, 0xA, regionLine(&lb, .{ .role = .manager, .slot = 0, .writer_id = 0xc1 }));
    const es = try reg.entries(testing.allocator);
    defer testing.allocator.free(es);
    try testing.expectEqual(@as(usize, 1), es.len);
    try testing.expectEqual(me.pid, es[0].pid);
    try testing.expectEqual(@as(usize, 2), es[0].n_regions);
    try testing.expectEqual(@as(u64, 0xb1), es[0].regions[0].writer_id);
    try testing.expectEqual(@as(usize, 1), es[0].n_retired);
    try testing.expectEqual(@as(u32, 3), es[0].retired[0].appender_id);
    try testing.expectEqual(@as(u64, 2), es[0].retired[0].term);
    try testing.expectEqual(@as(usize, 0), try reg.othersAlive(testing.allocator, me.pid));
    // One region out (its `r` line with it), the other stands.
    reg.removeRegion(me.pid, "jr", 0xb1, .any) catch {};
    {
        const one = try reg.entries(testing.allocator);
        defer testing.allocator.free(one);
        try testing.expectEqual(@as(usize, 1), one.len);
        try testing.expectEqual(@as(usize, 1), one[0].n_regions);
        try testing.expectEqual(@as(u64, 0xc2), one[0].regions[0].writer_id);
        try testing.expectEqual(@as(usize, 0), one[0].n_retired);
    }
    // A file a dead predecessor under this pid left (other start ticks) is
    // replaced at the next append, never extended.
    try reg.writeEntry(me.pid, "1\nj 9 00000000000000ee\n");
    try reg.append(me.pid, me.start_ticks, 0xA, regionLine(&lb, .{ .role = .joiner, .slot = 9, .writer_id = 0xb5 }));
    {
        const fresh = try reg.entries(testing.allocator);
        defer testing.allocator.free(fresh);
        try testing.expectEqual(@as(usize, 1), fresh.len);
        try testing.expectEqual(me.start_ticks, fresh[0].start_ticks);
        try testing.expectEqual(@as(usize, 1), fresh[0].n_regions);
        try testing.expectEqual(@as(u64, 0xb5), fresh[0].regions[0].writer_id);
    }
    reg.removeRegion(me.pid, "j", 0xb5, .any) catch {};
    const none = try reg.entries(testing.allocator);
    defer testing.allocator.free(none);
    try testing.expectEqual(@as(usize, 0), none.len);
}

test "B1 (review): a re-joined-in-place member's retired mark covers ONLY its retired incarnation's page — the successor incarnation (the same writer id) evicted later, its page Recovering, is STALE; while its page is Live it is current; the retired page alone is spared" {
    const node: u64 = 0xA;
    var e = Entry.parse(42, "77 000000000000000a\nj 9 00000000000000b1\nr 00000000000000b1 3 0 0\n").?;
    try testing.expectEqual(Verdict.stale, judge(&e, &.{ pg(3, .recovered, node, 0xb1), pg(5, .recovering, node, 0xb1) }, node, 0, false, .{}));
    try testing.expectEqual(Verdict.current, judge(&e, &.{ pg(3, .recovered, node, 0xb1), pg(5, .live, node, 0xb1) }, node, 0, false, .{}));
    try testing.expectEqual(Verdict.retired, judge(&e, &.{pg(3, .recovered, node, 0xb1)}, node, 0, false, .{}));
    // Round 3 major 1: the appender id comes back (the lowest Free pair) under
    // a later term — the old retirement does not cover the new page.
    var reused = Entry.parse(42, "77 000000000000000a\nj 9 00000000000000b1\nr 00000000000000b1 5 4 0\nr 00000000000000b1 7 6 0\n").?;
    try testing.expectEqual(Verdict.retired, judge(&reused, &.{ pgTerm(5, .recovered, node, 0xb1, 4), pgTerm(7, .recovered, node, 0xb1, 6) }, node, 0, false, .{}));
    try testing.expectEqual(Verdict.stale, judge(&reused, &.{ pgTerm(5, .recovering, node, 0xb1, 8), pgTerm(7, .recovered, node, 0xb1, 6) }, node, 0, false, .{}));
}

test "B3 (review): a joiner's departure never unregisters the host key while the reservation holder's key is ours — the manager re-mounting on this host holds it before it enrols" {
    var tb: [128]u8 = undefined;
    const base = try std.fmt.bufPrint(&tb, "/build/tmp/host-fence-b3-{d}", .{linux.getpid()});
    defer Io.Dir.cwd().deleteTree(testing.io, base) catch {};
    var reg = Registry.at(base, .set, 0xb3);
    reg.lock() catch return @import("../../core/testkit.zig").skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    defer reg.unlock();
    var ns = fake.Namespace.init(testing.allocator);
    defer ns.deinit();
    var h: fake.Host = .{ .ns = &ns, .host_id = "this-host-id-001".* };
    var c = try pr.Client.init(testing.allocator, h.face(), 1, 0x7777);
    defer c.deinit();
    try fencing.acquireManagerHold(&c);
    try testing.expect(!departUnregisters(&reg, testing.allocator, &c, proc.localIdentity().pid));
}

/// The B2 rig's page source: the sibling's page Live at the first read,
/// Recovering at every later one (a remote recovery's step 2 between the
/// judgement and the register).
const RacingPages = struct {
    node: u64,
    writer_id: u64,
    reads: u32 = 0,

    fn read(ctx: *anyopaque, gpa: Allocator) error{ Io, Corrupt, OutOfMemory }![]page.Page {
        const self: *RacingPages = @ptrCast(@alignCast(ctx));
        self.reads += 1;
        const out = try gpa.alloc(page.Page, 1);
        out[0] = pg(4, if (self.reads == 1) .live else .recovering, self.node, self.writer_id);
        return out;
    }
};

fn registerLadderFn(_: *anyopaque, c: *pr.Client) anyerror!void {
    _ = try c.registerLadder();
}

test "B2 (review): the rule re-judges AFTER the register, under the lock — a sibling whose page went Recovering between the judgement and the register (a remote recovery racing it) is killed, never left re-admitted" {
    const gpa = testing.allocator;
    var tb: [128]u8 = undefined;
    const base = try std.fmt.bufPrint(&tb, "/build/tmp/host-fence-b2-{d}", .{linux.getpid()});
    defer Io.Dir.cwd().deleteTree(testing.io, base) catch {};
    var reg = Registry.at(base, .set, 0xb2);
    reg.lock() catch return @import("../../core/testkit.zig").skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    defer reg.unlock();
    // The sibling: a live process of this host enrolled under a writer id
    // its start ticks recompute.
    var child = try std.process.spawn(testing.io, .{ .argv = &.{"cat"}, .stdin = .pipe, .stdout = .ignore, .stderr = .ignore });
    var reaped = false;
    defer if (!reaped) {
        _ = linux.kill(child.id.?, .KILL);
        _ = child.wait(testing.io) catch {};
    };
    const sib_pid: u32 = @intCast(child.id.?);
    const local = proc.localIdentity();
    const ticks = proc.startTicksOf(sib_pid) orelse return @import("../../core/testkit.zig").skip(@src(), .no_device, "/proc does not show the spawned sibling's start ticks");
    const sib: claim.Identity = .{ .boot_id = local.boot_id, .pid = sib_pid, .start_ticks = ticks, .hostname_hash = local.hostname_hash };
    var lb: [64]u8 = undefined;
    try reg.append(sib_pid, ticks, 0xA, regionLine(&lb, .{ .role = .joiner, .slot = 9, .writer_id = sib.writerId(9) }));
    // The device: another host holds WERO; this host was fenced (absent).
    var ns = fake.Namespace.init(gpa);
    defer ns.deinit();
    var mh: fake.Host = .{ .ns = &ns, .host_id = "other-host-id-01".* };
    var m = try pr.Client.init(gpa, mh.face(), 1, 0x1111);
    defer m.deinit();
    try fencing.acquireManagerHold(&m);
    var h: fake.Host = .{ .ns = &ns, .host_id = "this-host-id-001".* };
    var c = try pr.Client.init(gpa, h.face(), 1, 0x2222);
    defer c.deinit();
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    var pages: RacingPages = .{ .node = 0xA, .writer_id = sib.writerId(9) };
    const ctx: RuleContext = .{ .gpa = gpa, .env = &env, .path = "b2", .local = local, .node_token = 0xA, .vol_index = 0, .taking_manager = false };
    // The rig's node is no block device: the drain after the kill cannot be
    // proven, so the registration is refused loud — after the re-judge
    // withdrew it and killed the sibling.
    log.testing.expectErrorLogs(1);
    try testing.expectError(error.SiblingLive, registerUnderRule(ctx, &reg, &c, .{ .ctx = &pages, .read = &RacingPages.read }, .{ .ctx = undefined, .run = &registerLadderFn }));
    try log.testing.expectAllFired();
    // The sibling stale at the re-judge: killed; the registration withdrawn
    // before the kill and never restored.
    try testing.expect(!proc.isWriterIncarnation(local, sib_pid, 9, sib.writerId(9)));
    try testing.expect(!(try hostRegistered(&c)));
    try testing.expect(pages.reads >= 3);
    _ = try child.wait(testing.io);
    reaped = true;
}

test "round 3 major 1: a live manager's entry once its role is durable (`p` at registration, `m` at enrolment, appender 0's page Live under it) is CURRENT when no reservation stands (a target restart without PTPL): a co-located rule run before its heartbeat re-acquires never kills it" {
    const node: u64 = 0xA;
    var pm = Entry.parse(45, "80 000000000000000a\np 0 00000000000000c9\nm 0 00000000000000c9\n").?;
    try testing.expectEqual(Verdict.current, judge(&pm, &.{pg(0, .live, node, 0xc9)}, node, 0, false, .{}));
}

test "review 4 blocker 1: a joined line round-trips its page (appender id, term, volume); a re-joined successor under the same writer id keeps its lines when its predecessor's go (`removeRegion` by page), an unjoined line is replaced at the join" {
    var tb: [128]u8 = undefined;
    const base = try std.fmt.bufPrint(&tb, "/build/tmp/host-fence-joined-{d}", .{linux.getpid()});
    defer Io.Dir.cwd().deleteTree(testing.io, base) catch {};
    var reg = Registry.at(base, .set, 0x5678);
    reg.lock() catch return @import("../../core/testkit.zig").skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    defer reg.unlock();
    const me = proc.localIdentity();
    var lb: [96]u8 = undefined;
    // The predecessor joined at appender 4, term 6, retired; its successor
    // (the same writer id) not joined yet.
    try reg.append(me.pid, me.start_ticks, 0xA, regionLine(&lb, .{ .role = .joiner, .slot = 9, .writer_id = 0xd2, .joined = .{ .appender_id = 4, .term = 6, .vol = 1 } }));
    try reg.append(me.pid, me.start_ticks, 0xA, retiredLine(&lb, .{ .writer_id = 0xd2, .appender_id = 4, .term = 6, .vol = 1 }));
    try reg.append(me.pid, me.start_ticks, 0xA, regionLine(&lb, .{ .role = .joiner, .slot = 9, .writer_id = 0xd2 }));
    {
        const es = try reg.entries(testing.allocator);
        defer testing.allocator.free(es);
        try testing.expectEqual(@as(usize, 2), es[0].n_regions);
        try testing.expectEqual(@as(?Joined, .{ .appender_id = 4, .term = 6, .vol = 1 }), es[0].regions[0].joined);
        try testing.expectEqual(@as(?Joined, null), es[0].regions[1].joined);
    }
    // The successor's join replaces its unjoined line only.
    reg.removeRegion(me.pid, "j", 0xd2, .unjoined) catch {};
    try reg.append(me.pid, me.start_ticks, 0xA, regionLine(&lb, .{ .role = .joiner, .slot = 9, .writer_id = 0xd2, .joined = .{ .appender_id = 7, .term = 8, .vol = 1 } }));
    // The predecessor goes: its line and its `r` only.
    try reg.removeRegion(me.pid, "jlr", 0xd2, .{ .page = .{ .aid = 4, .vol = 1 } });
    const es = try reg.entries(testing.allocator);
    defer testing.allocator.free(es);
    try testing.expectEqual(@as(usize, 1), es[0].n_regions);
    try testing.expectEqual(@as(?Joined, .{ .appender_id = 7, .term = 8, .vol = 1 }), es[0].regions[0].joined);
    try testing.expectEqual(@as(usize, 0), es[0].n_retired);
}

test "review 5 minor 1: a retirement covers its page on ITS volume only — a member's volumes share its writer id and often its appender id: volume 0's `r` never spares volume 1's evicted page" {
    const node: u64 = 0xA;
    var e = Entry.parse(49, "84 000000000000000a\nj 9 00000000000000d2 4 6 1\nr 00000000000000d2 4 6 0\n").?;
    try testing.expectEqual(Verdict.stale, judge(&e, &.{pgTerm(4, .recovered, node, 0xd2, 6)}, node, 1, false, .{}));
}

test "review 5 minor 1: removing one volume's page lines leaves another volume's lines under the same writer id and appender id" {
    var tb: [128]u8 = undefined;
    const base = try std.fmt.bufPrint(&tb, "/build/tmp/host-fence-vols-{d}", .{linux.getpid()});
    defer Io.Dir.cwd().deleteTree(testing.io, base) catch {};
    var reg = Registry.at(base, .set, 0x9abc);
    reg.lock() catch return @import("../../core/testkit.zig").skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    defer reg.unlock();
    const me = proc.localIdentity();
    try reg.append(me.pid, me.start_ticks, 0xA, "j 9 00000000000000d2 4 6 0\n");
    try reg.append(me.pid, me.start_ticks, 0xA, "j 9 00000000000000d2 4 6 1\n");
    try reg.removeRegion(me.pid, "jlr", 0xd2, .{ .page = .{ .aid = 4, .vol = 0 } });
    const es = try reg.entries(testing.allocator);
    defer testing.allocator.free(es);
    try testing.expectEqual(@as(usize, 1), es.len);
    try testing.expectEqual(@as(usize, 1), es[0].n_regions);
    try testing.expectEqual(@as(u32, 1), es[0].regions[0].joined.?.vol);
}

test "review 5 minor 2: a joiner's line naming its page is appended BEFORE its unjoined line goes — an append that fails leaves the unjoined line, never a process with no line" {
    var tb: [128]u8 = undefined;
    const base = try std.fmt.bufPrint(&tb, "/build/tmp/host-fence-join-order-{d}", .{linux.getpid()});
    defer Io.Dir.cwd().deleteTree(testing.io, base) catch {};
    var reg = Registry.at(base, .set, 0xdef0);
    const me = proc.localIdentity();
    {
        reg.lock() catch return @import("../../core/testkit.zig").skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
        defer reg.unlock();
        try reg.append(me.pid, me.start_ticks, 0xA, "j 9 00000000000000d2\n");
    }
    var j: Joiner = .{ .gpa = testing.allocator, .device = undefined, .client = undefined, .reg = reg, .slot = 9, .writer_id = 0xd2, .vol = 0 };
    j.reg.test_fail_append = true;
    try testing.expectError(error.RegistryUnavailable, j.joined(4, 6));
    const es = try reg.entries(testing.allocator);
    defer testing.allocator.free(es);
    try testing.expectEqual(@as(usize, 1), es.len);
    try testing.expectEqual(@as(usize, 1), es[0].n_regions);
}

test "round 3 major 4, review 4 blocker 1b: a sibling mid-leave stays visible to the rule — its `l` line names its appender id, term and volume: Live under it is current; Recovering (frozen during its leave, evicted) is stale; a Free page naming it — only `releaseRecovered` leaves one (the leave's own Free page clears the identity) — is stale; Free with the identity cleared (its own landed leave, the line removed next) is stale too: its writes all landed, a kill there costs nothing" {
    const node: u64 = 0xA;
    var e = Entry.parse(46, "81 000000000000000a\nl 9 00000000000000d1 4 6 0\n").?;
    try testing.expectEqual(Verdict.stale, judge(&e, &.{pgTerm(4, .recovering, node, 0xd1, 6)}, node, 0, false, .{}));
    try testing.expectEqual(Verdict.current, judge(&e, &.{pgTerm(4, .live, node, 0xd1, 6)}, node, 0, false, .{}));
    try testing.expectEqual(Verdict.stale, judge(&e, &.{pgTerm(4, .free, node, 0xd1, 6)}, node, 0, false, .{}));
    try testing.expectEqual(Verdict.stale, judge(&e, &.{page.Page.free(4, 9, .{})}, node, 0, false, .{}));
}

test "review 4 blocker 1: a joined member's line names its appender id, term and volume (`j <slot> <writer id> <appender id> <term> <volume>`) — its pair released after its recovery and reused by a new joiner (the lowest Free pair) names no one: STALE, never joining; Live under its own writer id and term is current, under another term stale; a retirement covering its (appender id, term) spares it; a line without an appender id (before its JoinAppender) is joining; another volume's line is not judged here" {
    const node: u64 = 0xA;
    var e = Entry.parse(48, "83 000000000000000a\nj 9 00000000000000d2 4 6 0\n").?;
    try testing.expectEqual(Verdict.stale, judge(&e, &.{pgTerm(4, .live, node, 0xe7, 7)}, node, 0, false, .{}));
    try testing.expectEqual(Verdict.current, judge(&e, &.{pgTerm(4, .live, node, 0xd2, 6)}, node, 0, false, .{}));
    try testing.expectEqual(Verdict.stale, judge(&e, &.{pgTerm(4, .live, node, 0xd2, 8)}, node, 0, false, .{}));
    try testing.expectEqual(Verdict.stale, judge(&e, &.{pgTerm(4, .free, node, 0xd2, 6)}, node, 0, false, .{}));
    var r = Entry.parse(48, "83 000000000000000a\nj 9 00000000000000d2 4 6 0\nr 00000000000000d2 4 6 0\n").?;
    try testing.expectEqual(Verdict.retired, judge(&r, &.{pgTerm(4, .recovered, node, 0xd2, 6)}, node, 0, false, .{}));
    var pre = Entry.parse(48, "83 000000000000000a\nj 9 00000000000000d2\n").?;
    try testing.expectEqual(Verdict.joining, judge(&pre, &.{pgTerm(4, .live, node, 0xe7, 7)}, node, 0, false, .{}));
    var other = Entry.parse(48, "83 000000000000000a\nj 9 00000000000000d2 4 6 1\n").?;
    try testing.expect(judge(&other, &.{pgTerm(4, .live, node, 0xe7, 7)}, node, 0, false, .{}) != .stale);
}

test "decision 0003 §DJ, a co-located member's death: its recoverer on this host kills the process whose registry line names the dead page (appender id, term, volume) and proves it gone; a process whose own retirement covers that page is spared; a dead page naming another node (two hosts under one host identifier) holds the recovery" {
    const gpa = testing.allocator;
    var tb: [128]u8 = undefined;
    const base = try std.fmt.bufPrint(&tb, "/build/tmp/host-fence-dj-{d}", .{linux.getpid()});
    defer Io.Dir.cwd().deleteTree(testing.io, base) catch {};
    var probe = Registry.at(base, .set, 0xd1);
    probe.lock() catch return @import("../../core/testkit.zig").skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    probe.unlock();
    const local = proc.localIdentity();
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    const ctx: RuleContext = .{ .gpa = gpa, .env = &env, .path = "dj", .local = local, .node_token = 0xA, .vol_index = 0, .taking_manager = false, .label = "data volume" };
    // Two co-located members of this host whose lines name a Recovering page
    // (appender 3, term 2): W, dead, and R, re-joining in place — its own
    // retirement covers that page.
    var children: [2]std.process.Child = undefined;
    var reaped: [2]bool = .{ false, false };
    for (&children) |*ch| ch.* = try std.process.spawn(testing.io, .{ .argv = &.{"cat"}, .stdin = .pipe, .stdout = .ignore, .stderr = .ignore });
    defer for (&children, reaped) |*ch, done| if (!done) {
        _ = linux.kill(ch.id.?, .KILL);
        _ = ch.wait(testing.io) catch {};
    };
    var ids: [2]claim.Identity = undefined;
    for (children, &ids) |ch, *id| {
        const pid: u32 = @intCast(ch.id.?);
        const ticks = proc.startTicksOf(pid) orelse return @import("../../core/testkit.zig").skip(@src(), .no_device, "/proc does not show the spawned member's start ticks");
        id.* = .{ .boot_id = local.boot_id, .pid = pid, .start_ticks = ticks, .hostname_hash = local.hostname_hash };
    }
    const reg = Registry.at(base, .set, 0xd1);
    var lb: [96]u8 = undefined;
    const joined: Joined = .{ .appender_id = 3, .term = 2, .vol = 0 };
    try reg.append(ids[0].pid, ids[0].start_ticks, 0xA, regionLine(&lb, .{ .role = .joiner, .slot = 9, .writer_id = ids[0].writerId(9), .joined = joined }));
    try reg.append(ids[1].pid, ids[1].start_ticks, 0xA, regionLine(&lb, .{ .role = .joiner, .slot = 9, .writer_id = ids[1].writerId(9), .joined = joined }));
    try reg.append(ids[1].pid, ids[1].start_ticks, 0xA, retiredLine(&lb, .{ .writer_id = ids[1].writerId(9), .appender_id = 3, .term = 2, .vol = 0 }));
    // R's retired page: spared (its own retirement covers it).
    const r_dead = pgTerm(3, .recovering, 0xA, ids[1].writerId(9), 2);
    try testing.expect(killColocatedMember(ctx, reg, &r_dead, &.{}));
    try testing.expect(proc.isWriterIncarnation(local, ids[1].pid, 9, ids[1].writerId(9)));
    // W's page: W killed and proven gone.
    const kills0 = guard.stats.pr_stale_sibling_kills.load();
    const w_dead = pgTerm(3, .recovering, 0xA, ids[0].writerId(9), 2);
    try testing.expect(killColocatedMember(ctx, reg, &w_dead, &.{}));
    try testing.expectEqual(kills0 + 1, guard.stats.pr_stale_sibling_kills.load());
    _ = try children[0].wait(testing.io);
    reaped[0] = true;
    try testing.expect(!proc.isWriterIncarnation(local, ids[0].pid, 9, ids[0].writerId(9)));
    // A dead page of another node under this host's key: nothing to kill
    // here, nothing the device can fence — the recovery holds.
    log.testing.expectErrorLogs(1);
    const foreign = pgTerm(3, .recovering, 0xB, ids[1].writerId(9), 2);
    try testing.expect(!killColocatedMember(ctx, reg, &foreign, &.{}));
    try log.testing.expectAllFired();
}

test "review minor 2 (decision 0003 §DJ): a co-located member's death whose process is already proven dead — no kill — still drains every data node before the recovery proceeds (a gone process's writes can still sit in its block queues): a node whose queue cannot be read holds the recovery" {
    const gpa = testing.allocator;
    var tb: [128]u8 = undefined;
    const base = try std.fmt.bufPrint(&tb, "/build/tmp/host-fence-dj-drain-{d}", .{linux.getpid()});
    defer Io.Dir.cwd().deleteTree(testing.io, base) catch {};
    var probe = Registry.at(base, .set, 0xd2);
    probe.lock() catch return @import("../../core/testkit.zig").skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    probe.unlock();
    const local = proc.localIdentity();
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    const ctx: RuleContext = .{ .gpa = gpa, .env = &env, .path = "dj", .local = local, .node_token = 0xA, .vol_index = 0, .taking_manager = false, .label = "data volume" };
    // W, the co-located member whose line names the dead page, has exited.
    var w = try std.process.spawn(testing.io, .{ .argv = &.{"cat"}, .stdin = .pipe, .stdout = .ignore, .stderr = .ignore });
    const pid: u32 = @intCast(w.id.?);
    const ticks = proc.startTicksOf(pid) orelse {
        _ = linux.kill(w.id.?, .KILL);
        _ = w.wait(testing.io) catch {};
        return @import("../../core/testkit.zig").skip(@src(), .no_device, "/proc does not show the spawned member's start ticks");
    };
    _ = linux.kill(w.id.?, .KILL);
    _ = try w.wait(testing.io);
    const id: claim.Identity = .{ .boot_id = local.boot_id, .pid = pid, .start_ticks = ticks, .hostname_hash = local.hostname_hash };
    const reg = Registry.at(base, .set, 0xd2);
    var lb: [96]u8 = undefined;
    try reg.append(id.pid, id.start_ticks, 0xA, regionLine(&lb, .{ .role = .joiner, .slot = 9, .writer_id = id.writerId(9), .joined = .{ .appender_id = 3, .term = 2, .vol = 0 } }));
    // The data node: one whose block queue cannot be read.
    var nb: [160]u8 = undefined;
    const node = try std.fmt.bufPrint(&nb, "{s}/data-node", .{base});
    try Io.Dir.cwd().writeFile(testing.io, .{ .sub_path = node, .data = "" });
    const dead = pgTerm(3, .recovering, 0xA, id.writerId(9), 2);
    const kills0 = guard.stats.pr_stale_sibling_kills.load();
    log.testing.expectErrorLogs(1);
    try testing.expect(!killColocatedMember(ctx, reg, &dead, &.{node}));
    try log.testing.expectAllFired();
    // Nothing was killed (the registry proved W gone by its start ticks).
    try testing.expectEqual(kills0, guard.stats.pr_stale_sibling_kills.load());
}

test "a co-located member whose MAIN thread exited first (a zombie in /proc/<pid>/stat while another thread still runs, holds its files and can still issue I/O) is alive to the registry and the §DA proof: its recoverer KILLS it and awaits the whole group's exit — never proves it dead and leaves that thread running" {
    const gpa = testing.allocator;
    var tb: [128]u8 = undefined;
    const base = try std.fmt.bufPrint(&tb, "/build/tmp/host-fence-threads-{d}", .{linux.getpid()});
    defer Io.Dir.cwd().deleteTree(testing.io, base) catch {};
    var probe = Registry.at(base, .set, 0xd3);
    probe.lock() catch return @import("../../core/testkit.zig").skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    probe.unlock();
    const local = proc.localIdentity();
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    const ctx: RuleContext = .{ .gpa = gpa, .env = &env, .path = "threads", .local = local, .node_token = 0xA, .vol_index = 0, .taking_manager = false, .label = "data volume" };
    var cb: [160:0]u8 = undefined;
    const claim_path = try std.fmt.bufPrintSentinel(&cb, "{s}/claim", .{base}, 0);
    var w = try proc.ThreadedStandIn.start(claim_path.ptr);
    defer w.deinit();
    const pid: u32 = @intCast(w.pid);
    const ticks = proc.startTicksOf(pid).?;
    const id: claim.Identity = .{ .boot_id = local.boot_id, .pid = pid, .start_ticks = ticks, .hostname_hash = local.hostname_hash };
    const reg = Registry.at(base, .set, 0xd3);
    var lb: [96]u8 = undefined;
    try reg.append(pid, ticks, 0xA, regionLine(&lb, .{ .role = .joiner, .slot = 9, .writer_id = id.writerId(9), .joined = .{ .appender_id = 3, .term = 2, .vol = 0 } }));
    w.exitMainThread();
    try testing.expect(proc.flockHeldElsewhere(claim_path.ptr));
    const dead = pgTerm(3, .recovering, 0xA, id.writerId(9), 2);
    const kills0 = guard.stats.pr_stale_sibling_kills.load();
    try testing.expect(killColocatedMember(ctx, reg, &dead, &.{}));
    try testing.expectEqual(kills0 + 1, guard.stats.pr_stale_sibling_kills.load());
    // The whole group is gone: the claim's flock is free.
    try testing.expect(!proc.flockHeldElsewhere(claim_path.ptr));
}

test "a co-located member whose /proc/<pid>/stat the kernel refuses to open (EMFILE under fd exhaustion) is never pruned as dead: the registry keeps it, the §DA proof cannot rule it out, its pin cannot be proven, so the recovery HOLDS (loud) and nothing is killed" {
    const gpa = testing.allocator;
    var tb: [128]u8 = undefined;
    const base = try std.fmt.bufPrint(&tb, "/build/tmp/host-fence-unreadable-{d}", .{linux.getpid()});
    defer Io.Dir.cwd().deleteTree(testing.io, base) catch {};
    var probe = Registry.at(base, .set, 0xd4);
    probe.lock() catch return @import("../../core/testkit.zig").skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    probe.unlock();
    const local = proc.localIdentity();
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    const ctx: RuleContext = .{ .gpa = gpa, .env = &env, .path = "unreadable", .local = local, .node_token = 0xA, .vol_index = 0, .taking_manager = false, .label = "data volume" };
    var w = try std.process.spawn(testing.io, .{ .argv = &.{"cat"}, .stdin = .pipe, .stdout = .ignore, .stderr = .ignore });
    defer {
        _ = linux.kill(w.id.?, .KILL);
        _ = w.wait(testing.io) catch {};
    }
    const pid: u32 = @intCast(w.id.?);
    const ticks = proc.startTicksOf(pid).?;
    const id: claim.Identity = .{ .boot_id = local.boot_id, .pid = pid, .start_ticks = ticks, .hostname_hash = local.hostname_hash };
    const reg = Registry.at(base, .set, 0xd4);
    var lb: [96]u8 = undefined;
    try reg.append(pid, ticks, 0xA, regionLine(&lb, .{ .role = .joiner, .slot = 9, .writer_id = id.writerId(9), .joined = .{ .appender_id = 3, .term = 2, .vol = 0 } }));
    var fault: proc.TestSeams.StatFault = .{ .pid = pid, .errno = .MFILE };
    proc.test_seams.stat_fault.store(&fault, .release);
    defer proc.test_seams.stat_fault.store(null, .release);
    const dead = pgTerm(3, .recovering, 0xA, id.writerId(9), 2);
    const kills0 = guard.stats.pr_stale_sibling_kills.load();
    log.testing.expectErrorLogs(1);
    try testing.expect(!killColocatedMember(ctx, reg, &dead, &.{}));
    try log.testing.expectAllFired();
    try testing.expectEqual(kills0, guard.stats.pr_stale_sibling_kills.load());
    proc.test_seams.stat_fault.store(null, .release);
    // Still registered, still alive: nothing pruned it, nothing killed it.
    try testing.expect(proc.isWriterIncarnation(local, pid, 9, id.writerId(9)));
    const es = try reg.entries(gpa);
    defer gpa.free(es);
    try testing.expectEqual(@as(usize, 1), es.len);
}

test "decision 0004 §4.4 step 6: a symmetric takeover is one preempt-all — the successor is the only registrant under WEAR, and every other host id counts; a fresh namespace's preempt-all removes nobody and the hold may be released" {
    const gpa = testing.allocator;
    var ns = fake.Namespace.init(gpa);
    defer ns.deinit();
    const id_a: [16]u8 = "manager-host-id1".*;
    const id_b: [16]u8 = "joiner-host-id-2".*;
    const id_c: [16]u8 = "successor-host-3".*;
    var ha: fake.Host = .{ .ns = &ns, .host_id = id_a };
    var hb: fake.Host = .{ .ns = &ns, .host_id = id_b };
    var hc: fake.Host = .{ .ns = &ns, .host_id = id_c };
    var a = try pr.Client.init(gpa, ha.face(), 1, 0x1111);
    defer a.deinit();
    a.rtype = nvme.RTYPE_WEAR;
    _ = try a.registerLadder();
    const fresh0 = guard.stats.preempt_alls.load();
    const fresh = try takeSuccession(&a, "fake-meta");
    try testing.expectEqual(HoldOutcome.acquired, fresh);
    try testing.expectEqual(fresh0 + 1, guard.stats.preempt_alls.load());
    try testing.expectEqual(nvme.RTYPE_WEAR, ns.rtype);
    var b = try pr.Client.init(gpa, hb.face(), 1, 0x2222);
    defer b.deinit();
    _ = try b.registerLadder();
    var c = try pr.Client.init(gpa, hc.face(), 1, 0x3333);
    defer c.deinit();
    c.rtype = nvme.RTYPE_WEAR;
    _ = try c.registerLadder();
    const all0 = guard.stats.preempt_alls.load();
    const vic0 = guard.stats.preempt_all_victims.load();
    const took = try takeSuccession(&c, "fake-meta");
    try testing.expectEqual(HoldOutcome.preempted, took);
    try testing.expectEqual(all0 + 1, guard.stats.preempt_alls.load());
    try testing.expect(guard.stats.preempt_all_victims.load() >= vic0 + 2);
    try testing.expect(ns.isRegistered(0x3333));
    try testing.expect(!ns.isRegistered(0x1111));
    try testing.expect(!ns.isRegistered(0x2222));
    try testing.expectEqual(nvme.RTYPE_WEAR, ns.rtype);
}

// ---------------------------------------------------------------------------
// The blind-rule refusals (decision 0004 §4.4.2, KD19).
// ---------------------------------------------------------------------------

/// A parked child, optionally in a NEW UTS namespace, that has signalled it
/// finished `unshare`: rows F16 and F16b tell the two shapes apart by that
/// namespace, so the units need one of each.
const ParkedNs = struct {
    pid: linux.pid_t,
    hold: linux.fd_t,

    fn startIn(new_uts: bool) !ParkedNs {
        var hold_fds: [2]linux.fd_t = undefined;
        if (linux.errno(linux.pipe2(&hold_fds, .{ .CLOEXEC = true })) != .SUCCESS) return error.Pipe;
        var ready_fds: [2]linux.fd_t = undefined;
        if (linux.errno(linux.pipe2(&ready_fds, .{ .CLOEXEC = true })) != .SUCCESS) return error.Pipe;
        const rc = linux.fork();
        if (linux.errno(rc) != .SUCCESS) return error.Fork;
        if (rc == 0) {
            _ = linux.close(hold_fds[1]);
            _ = linux.close(ready_fds[0]);
            if (new_uts and linux.errno(linux.unshare(linux.CLONE.NEWUTS)) != .SUCCESS) linux.exit_group(3);
            const one: u8 = 1;
            _ = linux.write(ready_fds[1], @ptrCast(&one), 1);
            var byte: u8 = 0;
            _ = linux.read(hold_fds[0], @ptrCast(&byte), 1);
            linux.exit_group(0);
        }
        _ = linux.close(hold_fds[0]);
        _ = linux.close(ready_fds[1]);
        var done: u8 = 0;
        if (linux.errno(linux.read(ready_fds[0], @ptrCast(&done), 1)) != .SUCCESS or done != 1) {
            _ = linux.close(ready_fds[0]);
            return error.NotReady;
        }
        _ = linux.close(ready_fds[0]);
        return .{ .pid = @intCast(rc), .hold = hold_fds[1] };
    }

    fn ticks(self: ParkedNs) ?u64 {
        return switch (proc.startTicksAnyState(@intCast(self.pid))) {
            .ticks => |t| t,
            else => null,
        };
    }

    fn deinit(self: *ParkedNs) void {
        _ = linux.close(self.hold);
        _ = linux.kill(self.pid, .KILL);
        var status: i32 = 0;
        _ = linux.waitpid(self.pid, &status, 0);
    }
};

/// Removes one mirrored component: the leaf directory the unit made, or the
/// symlink that took its place (`rmdir` answers ENOTDIR for a symlink, `unlink`
/// EISDIR for a directory: one of the two is the answer).
fn rmLeaf(path: []const u8) void {
    var buf: [linux.PATH_MAX]u8 = undefined;
    const z = zpath(&buf, path);
    if (linux.errno(linux.rmdir(z.ptr)) != .SUCCESS) _ = linux.unlink(z.ptr);
}

/// `mkdir -p` for the two-view trees the registry-root units mirror.
fn mkdirsTo(path: []const u8) void {
    var buf: [linux.PATH_MAX]u8 = undefined;
    var i: usize = 1;
    while (i <= path.len) : (i += 1) {
        if (i != path.len and path[i] != '/') continue;
        const z = std.fmt.bufPrintSentinel(&buf, "{s}", .{path[0..i]}, 0) catch return;
        _ = linux.mkdir(z.ptr, 0o755);
    }
}

/// A registry-root verdict, told as its reason (null = the two views are the
/// same directory).
fn rootReason(v: RootVerdict) ?RefuseReason {
    return switch (v) {
        .same => null,
        .refused => |r| r.reason,
    };
}

fn rootErrno(v: RootVerdict) ?linux.E {
    return switch (v) {
        .same => null,
        .refused => |r| r.errno,
    };
}

test "the judgement is blind to another node token (decision 0004 §4.4.2): an entry whose header names another token, or none, is `.unjudgeable` — never current, retired, stale, and never killed for it — and `append` writes that token once, in the header" {
    const node: u64 = 0xA;
    // The same entry, judged by the host its own token names, is current;
    // judged by another host it has no verdict at all.
    var e = Entry.parse(42, "77 000000000000000b\nj 9 00000000000000b1\n").?;
    try testing.expectEqual(Verdict.current, judge(&e, &.{pg(3, .live, 0xB, 0xb1)}, 0xB, 0, false, .{}));
    try testing.expectEqual(Verdict.unjudgeable, judge(&e, &.{pg(3, .live, 0xB, 0xb1)}, node, 0, false, .{}));
    try testing.expectEqual(Verdict.unjudgeable, judge(&e, &.{}, node, 0, false, .{}));
    // A manager under another token is not stale either: the rule never kills
    // what it cannot read.
    var m = Entry.parse(45, "80 000000000000000b\nm 0 00000000000000c9\n").?;
    try testing.expectEqual(Verdict.stale, judge(&m, &.{pg(0, .live, 0xB, 0xc9)}, 0xB, 0, false, .{ .reserved = true }));
    try testing.expectEqual(Verdict.unjudgeable, judge(&m, &.{pg(0, .live, 0xB, 0xc9)}, node, 0, false, .{ .reserved = true }));
    // An entry with no token word (an older binary) is unjudgeable, and 0 is
    // never a token.
    var none = Entry.parse(42, "77\nj 9 00000000000000b1\n").?;
    try testing.expectEqual(@as(u64, 0), none.node_token);
    try testing.expectEqual(Verdict.unjudgeable, judge(&none, &.{pg(3, .live, node, 0xb1)}, node, 0, false, .{}));

    // `append` writes the header once, token included, and keeps the first
    // writer's across the entry's later lines.
    var tb: [128]u8 = undefined;
    const base = try std.fmt.bufPrint(&tb, "/build/tmp/host-fence-token-{d}", .{linux.getpid()});
    defer Io.Dir.cwd().deleteTree(testing.io, base) catch {};
    var reg = Registry.at(base, .set, 0xd19);
    reg.lock() catch return @import("../../core/testkit.zig").skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    defer reg.unlock();
    const me = proc.localIdentity();
    var lb: [64]u8 = undefined;
    try reg.append(me.pid, me.start_ticks, 0xA, regionLine(&lb, .{ .role = .joiner, .slot = 9, .writer_id = 0xb1 }));
    try reg.append(me.pid, me.start_ticks, 0xB, retiredLine(&lb, .{ .writer_id = 0xb1, .appender_id = 3, .term = 2, .vol = 0 }));
    var rb: [4096]u8 = undefined;
    const text = reg.readEntry(me.pid, &rb) orelse return error.Missing;
    var want: [64]u8 = undefined;
    const header = std.fmt.bufPrint(&want, "{d} 000000000000000a\n", .{me.start_ticks}) catch return error.Fmt;
    try testing.expect(std.mem.startsWith(u8, text, header));
    const back = Entry.parse(me.pid, text) orelse return error.Parse;
    try testing.expectEqual(@as(u64, 0xA), back.node_token);
    try testing.expectEqual(@as(usize, 1), back.n_regions);
    try testing.expectEqual(@as(usize, 1), back.n_retired);
}

test "blind rules (decision 0004 §4.4.2, KD19): a live sibling the rule cannot judge refuses the admission by name — `node_token_renamed` in this UTS namespace, `node_token_hostname` in another, `node_token_missing` with no token — each counted on its reason and on the total, nothing killed, and an entry under this process's token admits" {
    const skip = @import("../../core/testkit.zig").skip;
    var tb: [128]u8 = undefined;
    const base = try std.fmt.bufPrint(&tb, "/build/tmp/host-fence-blind-{d}", .{linux.getpid()});
    defer Io.Dir.cwd().deleteTree(testing.io, base) catch {};
    var reg = Registry.at(base, .set, 0xd19);
    reg.lock() catch return skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    defer reg.unlock();
    var same_ns = ParkedNs.startIn(false) catch return skip(@src(), .no_kernel_feature, "fork for the UTS-namespace fixture is refused");
    defer same_ns.deinit();
    var other_ns = ParkedNs.startIn(true) catch return skip(@src(), .no_kernel_feature, "unshare(CLONE_NEWUTS) is refused for this test process");
    defer other_ns.deinit();
    const st = same_ns.ticks() orelse return error.Ticks;
    const ot = other_ns.ticks() orelse return error.Ticks;
    // The discriminator reads real namespaces: the two children differ.
    const mine = proc.utsNamespaceIno(0) orelse return skip(@src(), .no_kernel_feature, "/proc/self/ns/uts is unreadable");
    const theirs = proc.utsNamespaceIno(@intCast(other_ns.pid)) orelse return skip(@src(), .no_kernel_feature, "the new UTS namespace is unreadable");
    try testing.expect(theirs != mine);
    try testing.expectEqual(mine, proc.utsNamespaceIno(@intCast(same_ns.pid)).?);

    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    const local = proc.localIdentity();
    const ctx: RuleContext = .{ .gpa = testing.allocator, .env = &env, .path = "blind", .local = local, .node_token = 0xA, .vol_index = 0, .taking_manager = false };
    var lb: [64]u8 = undefined;
    const total0 = guard.stats.pr_stale_sibling_refusals.load();
    log.testing.expectErrorLogs(3);

    // Another UTS namespace: two hostnames for one set on one host.
    try reg.append(@intCast(other_ns.pid), ot, 0xB, regionLine(&lb, .{ .role = .joiner, .slot = 9, .writer_id = 0xb1 }));
    const hostname0 = guard.stats.pr_stale_sibling_refusals_node_token_hostname.load();
    try testing.expectError(error.SiblingUnjudgeable, refuseUnjudgeableSiblings(ctx, &reg));
    try testing.expectEqual(hostname0 + 1, guard.stats.pr_stale_sibling_refusals_node_token_hostname.load());
    // The sibling shares this namespace, so the host's name changed while it
    // ran: the other reason, with the restart-or-restore remedy.
    reg.remove(@intCast(other_ns.pid));
    try reg.append(@intCast(same_ns.pid), st, 0xB, regionLine(&lb, .{ .role = .joiner, .slot = 9, .writer_id = 0xb2 }));
    const renamed0 = guard.stats.pr_stale_sibling_refusals_node_token_renamed.load();
    try testing.expectError(error.SiblingUnjudgeable, refuseUnjudgeableSiblings(ctx, &reg));
    try testing.expectEqual(renamed0 + 1, guard.stats.pr_stale_sibling_refusals_node_token_renamed.load());
    // No token at all: an older binary's entry, never judged under this token.
    reg.remove(@intCast(same_ns.pid));
    try reg.append(@intCast(same_ns.pid), st, 0, "j 9 00000000000000b3\n");
    const missing0 = guard.stats.pr_stale_sibling_refusals_node_token_missing.load();
    try testing.expectError(error.SiblingUnjudgeable, refuseUnjudgeableSiblings(ctx, &reg));
    try testing.expectEqual(missing0 + 1, guard.stats.pr_stale_sibling_refusals_node_token_missing.load());
    try log.testing.expectAllFired();
    try testing.expectEqual(total0 + 3, guard.stats.pr_stale_sibling_refusals.load());

    // Nothing was killed: both siblings still live, the rule met them and
    // refused. An entry under THIS token admits.
    try testing.expect(proc.incarnationAlive(@intCast(same_ns.pid), st));
    try testing.expect(proc.incarnationAlive(@intCast(other_ns.pid), ot));
    reg.remove(@intCast(same_ns.pid));
    try reg.append(@intCast(same_ns.pid), st, 0xA, regionLine(&lb, .{ .role = .joiner, .slot = 9, .writer_id = 0xb4 }));
    try refuseUnjudgeableSiblings(ctx, &reg);
    try testing.expectEqual(total0 + 3, guard.stats.pr_stale_sibling_refusals.load());
}

test "KD19's mapping of a failed registry-root comparison: the host's view having no such path is the private registry (`registry_root`, the shape a mismatch is); every other failure to compare reads the host's view as unreadable — EACCES and EPERM on /proc/1/root, ELOOP from the resolve flags, EXDEV, ENOSYS, anything else" {
    try testing.expectEqual(RefuseReason.registry_root, rootRefusal(.walk, .NOENT));
    for ([_]linux.E{ .ACCES, .PERM, .LOOP, .XDEV, .NOSYS, .INVAL, .NOLCK }) |e| {
        try testing.expectEqual(RefuseReason.registry_root_unreadable, rootRefusal(.walk, e));
        try testing.expectEqual(RefuseReason.registry_root_unreadable, rootRefusal(.host_root, e));
    }
    // pid 1's root itself missing is unreadable too: there is no host to see.
    try testing.expectEqual(RefuseReason.registry_root_unreadable, rootRefusal(.host_root, .NOENT));
    try testing.expectEqualStrings("registry_root", RefuseReason.registry_root.text());
    try testing.expectEqualStrings("registry_root_unreadable", RefuseReason.registry_root_unreadable.text());
    try testing.expectEqualStrings("node_token_hostname", RefuseReason.node_token_hostname.text());
    try testing.expectEqualStrings("node_token_renamed", RefuseReason.node_token_renamed.text());
    try testing.expectEqualStrings("node_token_missing", RefuseReason.node_token_missing.text());
}

test "blind rules (decision 0004 §4.4.2, KD19): the registry-root comparison over a two-view tree — the same inode passes, another directory or no such path in the host's view refuses as `registry_root`, an unreadable host root as `registry_root_unreadable`, a symlink that resolves differently refuses and one that resolves identically passes, and an ABSOLUTE symlink component resolves INSIDE the host's view where a plain `openat` falsely passes" {
    const skip = @import("../../core/testkit.zig").skip;
    // Every path has a plain slice and its own NUL-terminated twin: `zpath`
    // cannot print into the buffer its argument lives in.
    var tb: [linux.PATH_MAX]u8 = undefined;
    const base = std.fmt.bufPrint(&tb, "/build/tmp/host-fence-root-{d}", .{linux.getpid()}) catch return error.Fmt;
    var z: struct {
        base: [linux.PATH_MAX]u8,
        own: [linux.PATH_MAX]u8,
        alias: [linux.PATH_MAX]u8,
        proot: [linux.PATH_MAX]u8,
        mirror: [linux.PATH_MAX]u8,
    } = undefined;
    var own_b: [linux.PATH_MAX]u8 = undefined;
    const own = std.fmt.bufPrint(&own_b, "{s}/own/hosts", .{base}) catch return error.Fmt; // this daemon's registry root
    mkdirsTo(own);
    var alias_b: [linux.PATH_MAX]u8 = undefined;
    const alias = std.fmt.bufPrint(&alias_b, "{s}/alias", .{base}) catch return error.Fmt;
    var parent_b: [linux.PATH_MAX]u8 = undefined;
    const own_parent = std.fmt.bufPrint(&parent_b, "{s}/own", .{base}) catch return error.Fmt;
    // `alias` -> `own`'s parent: a symlinked component the walk meets either way.
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.symlink(zpath(&z.base, own_parent).ptr, zpath(&z.alias, alias).ptr)));
    var proot_b: [linux.PATH_MAX]u8 = undefined;
    const proot = std.fmt.bufPrint(&proot_b, "{s}/proot", .{base}) catch return error.Fmt; // the mirrored host root
    mkdirsTo(proot);
    var mir_b: [linux.PATH_MAX]u8 = undefined;
    const mirror = std.fmt.bufPrint(&mir_b, "{s}/{s}", .{ proot, own[1..] }) catch return error.Fmt; // the host's view of it
    var mir_dir_b: [linux.PATH_MAX]u8 = undefined;
    const mir_dir = std.fmt.bufPrint(&mir_dir_b, "{s}", .{mirror[0 .. mirror.len - "/hosts".len]}) catch return error.Fmt;

    // No such path in the host's view: a private `/run` the host never saw.
    const missing = judgeRootPaths(proot, own);
    try testing.expectEqual(RefuseReason.registry_root, rootReason(missing).?);
    try testing.expectEqual(linux.E.NOENT, rootErrno(missing).?);
    // The same path, another directory there.
    mkdirsTo(mirror);
    try testing.expectEqual(RefuseReason.registry_root, rootReason(judgeRootPaths(proot, own)).?);
    // A symlinked component that resolves differently in the host's view.
    rmLeaf(mirror);
    var away_b: [linux.PATH_MAX]u8 = undefined;
    const away = std.fmt.bufPrint(&away_b, "{s}/away", .{proot}) catch return error.Fmt;
    mkdirsTo(away);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.symlink("away", zpath(&z.mirror, mirror).ptr)));
    const diff = judgeRootPaths(proot, own);
    try testing.expectEqual(linux.E.NOENT, rootErrno(diff).?);
    try testing.expectEqual(RefuseReason.registry_root, rootReason(diff).?);
    // The host's root itself unreadable — the shape `hidepid` and an LSM give
    // EACCES and EPERM for: the comparison cannot answer, and that refuses.
    var gone_b: [linux.PATH_MAX]u8 = undefined;
    const gone = std.fmt.bufPrint(&gone_b, "{s}/no-such-host-root", .{base}) catch return error.Fmt;
    try testing.expectEqual(RefuseReason.registry_root_unreadable, rootReason(judgeRootPaths(gone, own)).?);
    // The host's real root: this daemon's path resolves identically in both
    // views — symlinked component and all — so it passes.
    try testing.expectEqual(@as(?RefuseReason, null), rootReason(judgeRootPaths("/", own)));
    try testing.expectEqual(@as(?RefuseReason, null), rootReason(judgeRootPaths("/", alias)));
    // The absolute symlink. The host's view meets `own`'s parent as an ABSOLUTE
    // symlink whose target is this daemon's own tree: `RESOLVE_IN_ROOT` walks it
    // inside the host's root, into another directory — refused. A plain `openat`
    // resolves that target from THIS process's root, lands on the daemon's own
    // registry, and falsely passes: the witness below says so.
    rmLeaf(mirror);
    rmLeaf(mir_dir);
    try testing.expectEqual(linux.E.SUCCESS, linux.errno(linux.symlink(zpath(&z.alias, alias).ptr, zpath(&z.mirror, mir_dir).ptr)));
    var mir_alias_b: [linux.PATH_MAX]u8 = undefined;
    const mir_alias = std.fmt.bufPrint(&mir_alias_b, "{s}/alias/hosts", .{mir_dir}) catch return error.Fmt;
    mkdirsTo(mir_alias); // what the absolute target resolves to INSIDE the host's root
    try testing.expectEqual(RefuseReason.registry_root, rootReason(judgeRootPaths(proot, own)).?);
    {
        const hf = linux.open(zpath(&z.proot, proot).ptr, .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .CLOEXEC = true }, 0);
        if (linux.errno(hf) != .SUCCESS) return skip(@src(), .other_step, "cannot open the two-view root");
        defer _ = linux.close(@intCast(hf));
        var rel_b: [linux.PATH_MAX]u8 = undefined;
        const plain = linux.openat(@intCast(hf), zpath(&rel_b, own[1..]).ptr, .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .CLOEXEC = true }, 0);
        if (linux.errno(plain) != .SUCCESS) return skip(@src(), .other_step, "the plain `openat` witness cannot walk the two-view tree");
        defer _ = linux.close(@intCast(plain));
        const ofd = linux.open(zpath(&z.own, own).ptr, .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .CLOEXEC = true }, 0);
        if (linux.errno(ofd) != .SUCCESS) return error.Open;
        defer _ = linux.close(@intCast(ofd));
        const ours = devIno(@intCast(ofd)) orelse return error.Open;
        const sees = devIno(@intCast(plain)) orelse return error.Open;
        try testing.expectEqual(ours.dev, sees.dev); // the false pass `openat2` refuses
        try testing.expectEqual(ours.ino, sees.ino);
    }
    // `RESOLVE_NO_MAGICLINKS`: the host's view of `/proc/self/root` walks a
    // procfs magic link. openat2 refuses that with ELOOP; a plain open follows
    // it, which is why this process's own view of the path succeeds. The host
    // root is `/` — the directory `/proc/1/root` opens — so the walked path is
    // `proc/self/root`, the magic link itself.
    const magic = judgeRootPaths("/", "/proc/self/root");
    try testing.expectEqual(RefuseReason.registry_root_unreadable, rootReason(magic).?);
    try testing.expectEqual(linux.E.LOOP, rootErrno(magic).?);
    // The registry seam exempts a rig that shares one root by construction; the
    // same mismatch without it refuses, logged by name with its remedy.
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try env.put("SQUEEZEFS_TEST_PID1_ROOT", proot);
    log.testing.expectErrorLogs(1);
    try testing.expect(!hostRegistryRootAgrees(&env, own, "fixture"));
    try log.testing.expectAllFired();
    try env.put("SQUEEZEFS_TEST_HOST_REGISTRY_DIR", base);
    try testing.expect(hostRegistryRootAgrees(&env, own, "fixture"));
    // `off` is the knob's documented default: pid 1's root, and this path passes.
    try env.put("SQUEEZEFS_TEST_PID1_ROOT", "off");
    try testing.expectEqual(@as(?RefuseReason, null), rootReason(judgeRootPaths(hostRootName(&env), own)));
}

test "blind rules (decision 0004 §4.4.2): a private MOUNT namespace that keeps the host's registry root passes (the same inode — systemd's `PrivateTmp=`, `ProtectSystem=` shape); a tmpfs over it — a container's own `/run` — refuses" {
    const skip = @import("../../core/testkit.zig").skip;
    var tb: [128]u8 = undefined;
    const base = try std.fmt.bufPrint(&tb, "/build/tmp/host-fence-mntns-{d}", .{linux.getpid()});
    defer Io.Dir.cwd().deleteTree(testing.io, base) catch {};
    var own_b: [linux.PATH_MAX]u8 = undefined;
    const own = std.fmt.bufPrint(&own_b, "{s}/own/hosts", .{base}) catch return error.Fmt;
    mkdirsTo(own);
    var fds: [2]linux.fd_t = undefined;
    if (linux.errno(linux.pipe2(&fds, .{ .CLOEXEC = true })) != .SUCCESS) return error.Pipe;
    const rc = linux.fork();
    if (linux.errno(rc) != .SUCCESS) return error.Fork;
    if (rc == 0) {
        _ = linux.close(fds[0]);
        const verdict: u8 = blk: {
            if (linux.errno(linux.unshare(linux.CLONE.NEWNS)) != .SUCCESS) break :blk 4;
            // Nothing this namespace mounts propagates back out.
            if (linux.errno(linux.mount(null, "/", null, linux.MS.REC | linux.MS.PRIVATE, 0)) != .SUCCESS) break :blk 5;
            // The namespace alone changes nothing: the registry is the same
            // directory in the host's view, so it passes.
            if (rootReason(judgeRootPaths("/proc/1/root", own)) != null) break :blk 1;
            // A tmpfs over it is a container's own `/run`: the siblings are
            // invisible, and that is the refusal.
            var tmp_b: [linux.PATH_MAX]u8 = undefined;
            if (linux.errno(linux.mount("tmpfs", zpath(&tmp_b, own).ptr, "tmpfs", 0, 0)) != .SUCCESS) break :blk 2;
            if (rootReason(judgeRootPaths("/proc/1/root", own)) != RefuseReason.registry_root) break :blk 3;
            break :blk 0;
        };
        var byte = verdict;
        _ = linux.write(fds[1], @ptrCast(&byte), 1);
        linux.exit_group(0);
    }
    _ = linux.close(fds[1]);
    var got: u8 = 9;
    if (linux.errno(linux.read(fds[0], @ptrCast(&got), 1)) != .SUCCESS) return error.Read;
    _ = linux.close(fds[0]);
    var status: i32 = 0;
    _ = linux.waitpid(@intCast(rc), &status, 0);
    if (got == 4 or got == 5) return skip(@src(), .no_kernel_feature, "unshare(CLONE_NEWNS) is refused for this test process");
    if (got == 2) return skip(@src(), .no_device, "a tmpfs cannot be mounted over the fixture's registry root");
    try testing.expectEqual(@as(u8, 0), got);
}
