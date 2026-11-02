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
//! its own retirement declaration (`r <writer id> <appender id>`: §BS's
//! re-joiner) and one still joining (no page names it; a manager whose role
//! is not durable yet) are spared: none holds a region a recoverer took.
//!
//! The candidates come from the host's registry of the set's mounts
//! (`<run>/hosts/set-<key>/<pid>`, keyed by the set and the wire host id: a
//! mount under another identity is another host, never a sibling); the same
//! registry shape counts the users of a shared fabric association
//! (`fabric-<key>`). Every mutation and every judgement runs under the
//! registry's flock, taken with a bounded wait that fails loud. A process
//! outside the initial pid namespace refuses to register (its siblings'
//! pids are not its own). Linux only.

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
/// flat volume), `p` a manager registered whose role is not yet (judged
/// joining), `j` a joiner.
pub const Role = enum { manager, pending, joiner };

/// One region an entry's process holds (its writer id at its slot).
pub const Region = struct { role: Role, slot: u32, writer_id: u64 };

/// One retired incarnation's page: the writer id and the appender id its
/// page carried when the latch failed (review B1: a §BS successor keeps
/// the writer id, never the appender id).
pub const Retired = struct { writer_id: u64, appender_id: u32 };

pub const MAX_REGIONS = 16; // one region per metadata volume a process writes: the set's member cap

pub const Entry = struct {
    pid: u32,
    start_ticks: u64,
    regions: [MAX_REGIONS]Region = undefined,
    n_regions: usize = 0,
    retired: [MAX_REGIONS]Retired = undefined,
    n_retired: usize = 0,

    pub fn regionSlice(self: *const Entry) []const Region {
        return self.regions[0..self.n_regions];
    }

    fn covers(self: *const Entry, writer_id: u64, appender_id: u32) bool {
        for (self.retired[0..self.n_retired]) |r| if (r.writer_id == writer_id and r.appender_id == appender_id) return true;
        return false;
    }

    /// The entry's text: the start ticks, then `m|p|j <slot> <writer id>`
    /// per region, `r <writer id> <appender id>` per retired incarnation
    /// (an `r` line without the appender id retires nothing).
    pub fn parse(pid: u32, text: []const u8) ?Entry {
        var lines = std.mem.tokenizeScalar(u8, text, '\n');
        const first = lines.next() orelse return null;
        var e: Entry = .{ .pid = pid, .start_ticks = std.fmt.parseInt(u64, std.mem.trim(u8, first, " "), 10) catch return null }; // proc(5)'s start ticks, decimal
        while (lines.next()) |line| {
            var f = std.mem.tokenizeScalar(u8, line, ' ');
            const tag = f.next() orelse continue;
            if (std.mem.eql(u8, tag, "r")) {
                const w = std.fmt.parseInt(u64, f.next() orelse continue, 16) catch continue; // a writer id, hex as `retiredLine` writes it
                const a = std.fmt.parseInt(u32, f.next() orelse continue, 10) catch continue; // an appender id, decimal as `retiredLine` writes it
                if (e.n_retired < MAX_REGIONS) {
                    e.retired[e.n_retired] = .{ .writer_id = w, .appender_id = a };
                    e.n_retired += 1;
                }
                continue;
            }
            const role: Role = if (std.mem.eql(u8, tag, "m")) .manager else if (std.mem.eql(u8, tag, "p")) .pending else if (std.mem.eql(u8, tag, "j")) .joiner else continue;
            const slot = std.fmt.parseInt(u32, f.next() orelse continue, 10) catch continue; // a mount slot, decimal as `regionLine` writes it
            const w = std.fmt.parseInt(u64, f.next() orelse continue, 16) catch continue; // a writer id, hex as `regionLine` writes it
            if (e.n_regions < MAX_REGIONS) {
                e.regions[e.n_regions] = .{ .role = role, .slot = slot, .writer_id = w };
                e.n_regions += 1;
            }
        }
        return e;
    }
};

pub const RegistryError = error{RegistryUnavailable};

/// A registry directory under the run dir (`/run/squeezefs/hosts` as root),
/// one file per process, one flock serializing every mutation and
/// judgement.
pub const Registry = struct {
    dir_buf: [linux.PATH_MAX]u8 = undefined,
    dir_len: usize = 0,
    kind: Kind = .set,
    lock_fd: linux.fd_t = -1,

    pub fn at(base: []const u8, kind: Kind, key: u64) Registry {
        var r: Registry = .{ .kind = kind };
        r.dir_len = (std.fmt.bufPrint(&r.dir_buf, "{s}/{s}-{x:0>16}", .{ base, kind.prefix(), key }) catch unreachable).len;
        return r;
    }

    pub fn runtime(kind: Kind, key: u64) Registry {
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

    pub fn unlock(self: *Registry) void {
        if (self.lock_fd < 0) return;
        _ = linux.close(self.lock_fd);
        self.lock_fd = -1;
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
    /// is replaced, never extended (review major 10).
    pub fn append(self: *const Registry, pid: u32, start_ticks: u64, line: []const u8) RegistryError!void {
        try self.mkdirs();
        var rb: [4096]u8 = undefined;
        var out: [4096 + 128]u8 = undefined;
        var w: Io.Writer = .fixed(&out);
        const old = self.readEntry(pid, &rb);
        const ours = if (old) |t| if (Entry.parse(pid, t)) |e| e.start_ticks == start_ticks else false else false;
        if (ours) w.writeAll(old.?) catch return error.RegistryUnavailable else w.print("{d}\n", .{start_ticks}) catch return error.RegistryUnavailable;
        w.writeAll(line) catch return error.RegistryUnavailable;
        try self.writeEntry(pid, w.buffered());
    }

    /// Drop this process's lines naming `writer_id` under `tags` (one
    /// region's — review major 10: never the whole file while another
    /// region of the process stands); the file goes with its last line.
    pub fn removeRegion(self: *const Registry, pid: u32, tags: []const u8, writer_id: u64) void {
        var rb: [4096]u8 = undefined;
        const old = self.readEntry(pid, &rb) orelse return;
        var out: [4096]u8 = undefined;
        var w: Io.Writer = .fixed(&out);
        var lines = std.mem.tokenizeScalar(u8, old, '\n');
        const header = lines.next() orelse return self.remove(pid);
        w.print("{s}\n", .{header}) catch return;
        var kept: usize = 0;
        while (lines.next()) |line| {
            var f = std.mem.tokenizeScalar(u8, line, ' ');
            const tag = f.next() orelse continue;
            const drop = tag.len == 1 and std.mem.indexOfScalar(u8, tags, tag[0]) != null and blk: {
                if (tag[0] != 'r') _ = f.next();
                const w_text = f.next() orelse break :blk false;
                break :blk (std.fmt.parseInt(u64, w_text, 16) catch break :blk false) == writer_id; // a writer id, hex as written
            };
            if (drop) continue;
            w.print("{s}\n", .{line}) catch return;
            kept += 1;
        }
        self.writeEntry(pid, if (kept == 0) "" else w.buffered()) catch {};
    }

    /// The whole entry (a dead process's, or a fabric association's user).
    pub fn remove(self: *const Registry, pid: u32) void {
        var pb: [linux.PATH_MAX]u8 = undefined;
        _ = linux.unlink(self.entryPath(&pb, pid).ptr);
    }

    /// The LIVE entries (a process whose start ticks still match; a dead
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
            if (proc.startTicksOf(pid) != e.start_ticks) {
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
    };
    return std.fmt.bufPrint(buf, "{s} {d} {x:0>16}\n", .{ tag, r.slot, r.writer_id }) catch unreachable;
}

pub fn retiredLine(buf: []u8, r: Retired) []const u8 {
    return std.fmt.bufPrint(buf, "r {x:0>16} {d}\n", .{ r.writer_id, r.appender_id }) catch unreachable;
}

// ---------------------------------------------------------------------------
// The judgement (pure).
// ---------------------------------------------------------------------------

pub const Verdict = enum {
    /// A Live page names its incarnation: its region is its own.
    current,
    /// Every non-Live page naming it is covered by its own retirement.
    retired,
    /// No page names it yet (or its manager role is not durable yet): it
    /// holds no region a recoverer took.
    joining,
    /// A recoverer took one of its regions (or the manager role passed):
    /// it must be dead before the host is admitted.
    stale,
};

/// The verdict of one sibling's entry against one volume's pages (appender
/// 0 = the manager's; `vol_index` = the volume's lane index, a manager's
/// slot on it). `taking_manager` = the registering mount is winning the D0
/// role on this volume: appender 0's page names a superseded holder.
pub fn judge(e: *const Entry, pages: []const page.Page, node_token: u64, vol_index: u32, taking_manager: bool) Verdict {
    var verdict: Verdict = .joining;
    for (e.regionSlice()) |r| {
        const v = judgeRegion(e, r, pages, node_token, vol_index, taking_manager) orelse continue;
        switch (v) {
            .stale => return .stale,
            .current => verdict = .current,
            .retired => if (verdict != .current) {
                verdict = .retired;
            },
            .joining => {},
        }
    }
    return verdict;
}

fn judgeRegion(e: *const Entry, r: Region, pages: []const page.Page, node_token: u64, vol_index: u32, taking_manager: bool) ?Verdict {
    switch (r.role) {
        .pending => return if (r.slot == vol_index) .joining else null,
        .manager => {
            if (r.slot != vol_index) return null;
            if (!taking_manager) for (pages) |p| {
                if (p.appender_id == 0 and p.state == .live and p.identity.node_token == node_token and p.identity.writer_id == r.writer_id) return .current;
            };
            return .stale;
        },
        .joiner => {
            var named = false;
            var uncovered = false;
            for (pages) |p| {
                if (p.appender_id == 0 or p.identity.node_token != node_token or p.identity.writer_id != r.writer_id) continue;
                if (p.state == .live) return .current;
                named = true;
                if (!e.covers(r.writer_id, p.appender_id)) uncovered = true;
            }
            if (uncovered) return .stale;
            return if (named) .retired else .joining;
        },
    }
}

// ---------------------------------------------------------------------------
// The rule.
// ---------------------------------------------------------------------------

pub const RuleContext = struct {
    gpa: Allocator,
    env: *const std.process.Environ.Map,
    /// The volume's block node (the log's name; its queue's in-flight
    /// writes are the drain's word).
    path: []const u8,
    local: claim.Identity,
    node_token: u64,
    vol_index: u32,
    taking_manager: bool,
};

/// How long a SIGKILLed sibling may take to exit (an I/O it sleeps in must
/// complete first) before the registration is refused.
pub const KILL_WAIT_MS: i32 = 10_000;

/// How long a killed sibling's writes may take to leave the namespace's
/// block queue (a reconnecting controller requeues them) before the
/// registration is refused.
pub const DRAIN_WAIT_MS: u64 = 10_000;

pub const RuleError = RegistryError || Allocator.Error || error{SiblingLive};

/// Under the registry's lock: every live sibling judged against `pages`; a
/// stale one proven dead or killed. Returns how many it killed.
/// `error.SiblingLive` = refused (logged by name, counted).
pub fn clearStaleSiblings(ctx: RuleContext, reg: *const Registry, pages: []const page.Page) RuleError!u32 {
    const es = try reg.entries(ctx.gpa);
    defer ctx.gpa.free(es);
    var killed: u32 = 0;
    for (es) |*e| {
        if (e.pid == ctx.local.pid) continue;
        const v = judge(e, pages, ctx.node_token, ctx.vol_index, ctx.taking_manager);
        if (v != .stale) {
            log.info("meta volume {s}: sibling pid {d} of this set on this host is {s} — spared by the host's registration (its region is its own)", .{ ctx.path, e.pid, @tagName(v) });
            continue;
        }
        if (!proveIncarnationAlive(ctx, e)) {
            guard.stats.pr_siblings_proven_dead.inc();
            reg.remove(e.pid);
            continue;
        }
        if (killSibling(ctx, e)) |why| {
            guard.stats.pr_stale_sibling_refusals.inc();
            log.err("meta volume {s}: REFUSING to register this host: the stale sibling pid {d} (writer id {x:0>16}) of this set on this host is alive and {s} — a registration would re-admit it to the device its region's recoverer owns; kill it by hand (`kill -9 {d}`) and remount", .{ ctx.path, e.pid, e.regionSlice()[0].writer_id, why, e.pid });
            return error.SiblingLive;
        }
        guard.stats.pr_stale_sibling_kills.inc();
        killed += 1;
        reg.remove(e.pid);
        log.warn("meta volume {s}: killed the stale sibling pid {d} (writer id {x:0>16}) of this set on this host before registering the host — its region was recovered, a registration would have re-admitted its writes", .{ ctx.path, e.pid, e.regionSlice()[0].writer_id });
    }
    return killed;
}

/// Is any live sibling stale against `pages` (no kill)?
fn anyStaleAlive(ctx: RuleContext, reg: *const Registry, pages: []const page.Page) RuleError!bool {
    const es = try reg.entries(ctx.gpa);
    defer ctx.gpa.free(es);
    for (es) |*e| {
        if (e.pid == ctx.local.pid) continue;
        if (judge(e, pages, ctx.node_token, ctx.vol_index, ctx.taking_manager) == .stale and proveIncarnationAlive(ctx, e)) return true;
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

/// SIGKILL through a pidfd (the start ticks re-read on it: never another
/// process under a recycled pid) and the exit awaited on the pidfd. Null =
/// dead; otherwise why not.
fn killSibling(ctx: RuleContext, e: *const Entry) ?[]const u8 {
    if (killRefusedSeam(ctx.env)) return "SQUEEZEFS_TEST_SIBLING_KILL_REFUSED denies its kill";
    const rc = linux.pidfd_open(@intCast(e.pid), 0);
    switch (linux.errno(rc)) {
        .SUCCESS => {},
        .SRCH => return null,
        else => return "cannot be opened by pidfd",
    }
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    if (proc.startTicksOf(e.pid) != e.start_ticks) return null;
    switch (linux.errno(linux.pidfd_send_signal(fd, .KILL, null, 0))) {
        .SUCCESS => {},
        .SRCH => return null,
        else => return "refuses SIGKILL",
    }
    var pfd = [1]linux.pollfd{.{ .fd = fd, .events = linux.POLL.IN, .revents = 0 }};
    while (true) {
        const pr_rc = linux.poll(&pfd, 1, KILL_WAIT_MS);
        switch (linux.errno(pr_rc)) {
            .SUCCESS => return if (pr_rc == 0) "did not exit within 10 s of SIGKILL (an I/O it sleeps in has not completed)" else null,
            .INTR => continue,
            else => return "cannot be awaited",
        }
    }
}

/// The namespace's block queue holds no write in flight (`/sys/class/block/
/// <node>/inflight`'s second word): a killed sibling's submitted writes —
/// a reconnecting controller's requeue included — have left it (review
/// major 9). Waited at most `DRAIN_WAIT_MS`; a node without the attribute
/// (not a block device) has nothing to drain.
fn awaitWriteDrain(ctx: RuleContext) error{SiblingLive}!void {
    const base = std.fs.path.basename(ctx.path);
    var pb: [linux.PATH_MAX]u8 = undefined;
    const attr = std.fmt.bufPrintSentinel(&pb, "/sys/class/block/{s}/inflight", .{base}, 0) catch return;
    const deadline = clock.monotonicNs() +| DRAIN_WAIT_MS * std.time.ns_per_ms;
    var backoff_ns: u64 = std.time.ns_per_ms;
    while (true) {
        var tb: [64]u8 = undefined;
        const text = proc.readSmall(attr.ptr, &tb) orelse return;
        var f = std.mem.tokenizeAny(u8, text, " \t\n");
        _ = f.next();
        const writes = std.fmt.parseInt(u64, f.next() orelse return, 10) catch return; // the attribute's writes word, decimal
        if (writes == 0) return;
        const now = clock.monotonicNs();
        if (now >= deadline) {
            guard.stats.pr_stale_sibling_refusals.inc();
            log.err("meta volume {s}: REFUSING to register this host: {d} write(s) stay in flight on {s} {d} ms after a stale sibling's kill — a registration would re-admit them", .{ ctx.path, writes, base, DRAIN_WAIT_MS });
            return error.SiblingLive;
        }
        const ts: linux.timespec = .{ .sec = 0, .nsec = @intCast(@min(backoff_ns, deadline - now)) };
        _ = linux.nanosleep(&ts, null);
        backoff_ns = @min(backoff_ns * 2, 100 * std.time.ns_per_ms); // a drain poll's ceiling: a requeue resolves in ≥ seconds
    }
}

/// This process runs in the initial pid namespace: the registry's pids are
/// the kernel's own, so a sibling's entry is judged against its process
/// (review major 8). `PROC_PID_INIT_INO`.
fn initialPidNamespace() bool {
    var st: linux.Statx = undefined;
    if (linux.errno(linux.statx(linux.AT.FDCWD, "/proc/self/ns/pid", 0, .{ .INO = true }, &st)) != .SUCCESS) return false;
    return st.ino == 0xEFFFFFFC; // PROC_PID_INIT_INO (include/linux/proc_ns.h)
}

/// Is this host registered under `c.key` (a registrant with our wire host
/// id and our key)? A registration there is idempotent: it re-admits nobody.
pub fn hostRegistered(c: *pr.Client) pr.Error!bool {
    const me = try c.wireHostId();
    var rep = try c.report();
    defer rep.deinit(c.gpa);
    for (rep.registrants) |r| if (std.mem.eql(u8, &r.host_id, &me) and r.rkey == c.key) return true;
    return false;
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

pub const AdmitError = RuleError || error{ PagesUnreadable, RegisterFailed, NamespaceBlind };

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
pub fn registerUnderRule(ctx: RuleContext, reg: *const Registry, c: *pr.Client, pages: PageSource, register: Register) AdmitError!bool {
    if (hostRegistered(c) catch false) return false;
    if (!initialPidNamespace()) {
        guard.stats.pr_stale_sibling_refusals.inc();
        log.err("meta volume {s}: REFUSING to register this host: this daemon runs outside the initial pid namespace — the host's registry names its siblings by pids it cannot judge (a sibling in another namespace would be re-admitted unseen)", .{ctx.path});
        return error.NamespaceBlind;
    }
    var round: u32 = 0;
    while (round < REJUDGE_ROUNDS) : (round += 1) {
        {
            const ps = try pages.get(ctx.gpa);
            defer ctx.gpa.free(ps);
            if (try clearStaleSiblings(ctx, reg, ps) > 0) try awaitWriteDrain(ctx);
        }
        register.run(register.ctx, c) catch return error.RegisterFailed;
        const ps = try pages.get(ctx.gpa);
        defer ctx.gpa.free(ps);
        if (!try anyStaleAlive(ctx, reg, ps)) return true;
        log.warn("meta volume {s}: a sibling of this set on this host turned stale between the rule and the register (a remote recovery raced it) — the host's registration withdrawn before it is killed", .{ctx.path});
        c.unregister(c.key) catch {
            log.err("meta volume {s}: REFUSING the registration: it could not be withdrawn from a stale sibling's host", .{ctx.path});
            return error.SiblingLive;
        };
    }
    guard.stats.pr_stale_sibling_refusals.inc();
    log.err("meta volume {s}: REFUSING to register this host: siblings kept turning stale across {d} registers", .{ ctx.path, REJUDGE_ROUNDS });
    return error.SiblingLive;
}

/// A clean departure: does it take the host's registration with it? Only
/// when no other process of the set on this host lives AND no reservation
/// stands under our key (review B3: under Write Exclusive – All
/// Registrants every registrant holds; under any type the holder's key may
/// be ours — a co-located manager re-mounting holds it before it enrols).
pub fn departUnregisters(reg: *const Registry, gpa: Allocator, c: *pr.Client, pid: u32) bool {
    const others = reg.othersAlive(gpa, pid) catch return false;
    if (others != 0) return false;
    var rep = c.report() catch return false;
    defer rep.deinit(c.gpa);
    if (rep.rtype == 0) return true;
    if (nvme.allRegistrants(rep.rtype)) {
        for (rep.registrants) |r| if (r.rkey == c.key) return false;
        return true;
    }
    return rep.holder() != c.key;
}

/// What a writer's registration rides: its key (the host's, or the
/// incarnation's under the seam) and the set's registry (null under the
/// seam).
pub const Plan = struct {
    key: u64,
    reg: ?Registry,
};

/// The host's key and registry from the WIRE host id the namespace answers
/// (review major 7: a configured hostid's spelling, an explicit pair other
/// than the metadata controller's, never splits one device registrant into
/// two keys). No wire id (FID 0x81 refused or zero) ⇒ the incarnation's
/// key and no rule — logged.
pub fn plan(env: *const std.process.Environ.Map, c: *pr.Client, set_uuid: [16]u8, incarnation_key: u64, path: []const u8) Plan {
    if (perMountIdentity(env)) return .{ .key = incarnation_key, .reg = null };
    const wire = c.wireHostId() catch {
        log.warn("meta volume {s}: the namespace answers no host identifier (FID 0x81) — the reservation key is this incarnation's and the stale-sibling rule cannot run", .{path});
        return .{ .key = incarnation_key, .reg = null };
    };
    return .{ .key = hostKey(&wire, set_uuid), .reg = Registry.runtime(.set, setRegistryKey(&wire, set_uuid)) };
}

fn joinRegisterFn(ctx: *anyopaque, c: *pr.Client) anyerror!void {
    const env: *const std.process.Environ.Map = @ptrCast(@alignCast(ctx));
    _ = try fencing.joinAsRegistrant(c, fencing.declaredRegistrantCap(env));
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
        const fresh = registerUnderRule(ctx, &reg, c, pages, .{ .ctx = @constCast(ctx.env), .run = &joinRegisterFn }) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            error.SiblingLive, error.NamespaceBlind => return error.HostAdmissionRefused,
            error.RegisterFailed => {
                log.err("meta volume {s}: this host's registration (key {x:0>16}) failed — refusing the join", .{ ctx.path, c.key });
                return error.HostAdmissionRefused;
            },
            error.RegistryUnavailable, error.PagesUnreadable => {
                log.err("meta volume {s}: the stale-sibling rule could not read its inputs ({s}) — refusing to register this host", .{ ctx.path, @errorName(e) });
                return error.HostAdmissionRefused;
            },
        };
        var lb: [64]u8 = undefined;
        reg.append(ctx.local.pid, ctx.local.start_ticks, regionLine(&lb, .{ .role = .joiner, .slot = slot, .writer_id = writer_id })) catch {
            log.err("meta volume {s}: this mount could not enter the host's registry of the set ({s}) — refusing the join", .{ ctx.path, reg.dir() });
            // The registration this admission made is taken back with it.
            if (fresh and departUnregisters(&reg, ctx.gpa, c, ctx.local.pid)) c.unregister(c.key) catch {};
            return error.HostAdmissionRefused;
        };
        if (fresh) {
            guard.stats.pr_host_registrations.inc();
            log.info("meta volume {s}: registered this host on the namespace (key {x:0>16}) — every mount of the set on this host rides it", .{ ctx.path, c.key });
        } else {
            guard.stats.pr_host_adoptions.inc();
            log.info("meta volume {s}: this host is registered on the namespace under the set's key {x:0>16} — adopted (no device mutation)", .{ ctx.path, c.key });
        }
        const self = try ctx.gpa.create(Joiner);
        self.* = .{ .gpa = ctx.gpa, .device = dev, .client = c, .reg = reg, .slot = slot, .writer_id = writer_id };
        return self;
    }

    /// The incarnation retired (its latch failed): declared with its page's
    /// appender id, so a sibling's registration spares this process while
    /// it re-joins — and only that page (review B1).
    pub fn declareRetired(self: *Joiner, appender_id: u32) void {
        self.reg.lock() catch return;
        defer self.reg.unlock();
        var lb: [64]u8 = undefined;
        const me = proc.localIdentity();
        self.reg.append(me.pid, me.start_ticks, retiredLine(&lb, .{ .writer_id = self.writer_id, .appender_id = appender_id })) catch {};
    }

    /// Before the clean leave: this region out of the registry (a leaving
    /// incarnation's Free page must never read as a stale sibling's).
    pub fn depart(self: *Joiner) void {
        self.reg.lock() catch return;
        defer self.reg.unlock();
        self.reg.removeRegion(proc.localIdentity().pid, "jr", self.writer_id);
    }

    /// The last process of the set on this host takes the host's
    /// registration with it when no reservation stands under the key (zero
    /// residue); otherwise it stays the siblings'.
    pub fn deinit(self: *Joiner) void {
        if (self.reg.lock()) |_| {
            defer self.reg.unlock();
            const pid = proc.localIdentity().pid;
            self.reg.removeRegion(pid, "jr", self.writer_id);
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
    var p = page.Page.free(id, 1, .{});
    p.state = state;
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

test "the judgement: a Live page names a current sibling; a Recovering, Recovered or Free page under its writer id is stale unless it declared the incarnation retired; no page yet is joining; the manager is current only on appender 0's Live page, stale once the role passed or while this mount takes it over; another node's page never names a sibling" {
    const node: u64 = 0xA;
    var e = Entry.parse(42, "77\nj 9 00000000000000b1\n").?;
    try testing.expectEqual(Verdict.joining, judge(&e, &.{}, node, 0, false));
    try testing.expectEqual(Verdict.current, judge(&e, &.{pg(3, .live, node, 0xb1)}, node, 0, false));
    try testing.expectEqual(Verdict.stale, judge(&e, &.{pg(3, .recovered, node, 0xb1)}, node, 0, false));
    try testing.expectEqual(Verdict.stale, judge(&e, &.{pg(3, .recovering, node, 0xb1)}, node, 0, false));
    try testing.expectEqual(Verdict.stale, judge(&e, &.{pg(3, .free, node, 0xb1)}, node, 0, false));
    // A re-join in place: the old page Recovered, the new one Live.
    try testing.expectEqual(Verdict.current, judge(&e, &.{ pg(3, .recovered, node, 0xb1), pg(5, .live, node, 0xb1) }, node, 0, false));
    // Another node's page under the same writer id is not this sibling's.
    try testing.expectEqual(Verdict.joining, judge(&e, &.{pg(3, .recovered, 0xB, 0xb1)}, node, 0, false));
    var r = Entry.parse(43, "78\nj 9 00000000000000b2\nr 00000000000000b2 4\n").?;
    try testing.expectEqual(Verdict.retired, judge(&r, &.{pg(4, .recovered, node, 0xb2)}, node, 0, false));
    // An `r` line without the appender id retires nothing.
    var bare = Entry.parse(43, "78\nj 9 00000000000000b2\nr 00000000000000b2\n").?;
    try testing.expectEqual(Verdict.stale, judge(&bare, &.{pg(4, .recovered, node, 0xb2)}, node, 0, false));
    // A manager registered whose role is not durable yet: joining.
    var p = Entry.parse(45, "80\np 0 00000000000000c9\n").?;
    try testing.expectEqual(Verdict.joining, judge(&p, &.{pg(0, .live, node, 0xd1)}, node, 0, false));
    var pm = Entry.parse(45, "80\np 0 00000000000000c9\nm 0 00000000000000c9\n").?;
    try testing.expectEqual(Verdict.stale, judge(&pm, &.{pg(0, .live, node, 0xd1)}, node, 0, false));
    try testing.expectEqual(Verdict.current, judge(&pm, &.{pg(0, .live, node, 0xc9)}, node, 0, false));
    var m = Entry.parse(44, "79\nm 0 00000000000000c1\nm 1 00000000000000c2\n").?;
    try testing.expectEqual(Verdict.current, judge(&m, &.{pg(0, .live, node, 0xc1)}, node, 0, false));
    try testing.expectEqual(Verdict.stale, judge(&m, &.{pg(0, .live, node, 0xd1)}, node, 0, false));
    try testing.expectEqual(Verdict.stale, judge(&m, &.{pg(0, .live, node, 0xc1)}, node, 0, true));
    // Volume 1: only the manager region at slot 1 is judged.
    try testing.expectEqual(Verdict.current, judge(&m, &.{pg(0, .live, node, 0xc2)}, node, 1, false));
    try testing.expect(Entry.parse(1, "") == null);
    try testing.expect(Entry.parse(1, "x\n") == null);
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
    try reg.append(me.pid, me.start_ticks, regionLine(&lb, .{ .role = .joiner, .slot = 9, .writer_id = 0xb1 }));
    try reg.append(me.pid, me.start_ticks, retiredLine(&lb, .{ .writer_id = 0xb1, .appender_id = 3 }));
    try reg.append(me.pid, me.start_ticks, regionLine(&lb, .{ .role = .manager, .slot = 1, .writer_id = 0xc2 }));
    // A dead process (pid 1's start ticks never match 1).
    try reg.append(1, 1, regionLine(&lb, .{ .role = .manager, .slot = 0, .writer_id = 0xc1 }));
    const es = try reg.entries(testing.allocator);
    defer testing.allocator.free(es);
    try testing.expectEqual(@as(usize, 1), es.len);
    try testing.expectEqual(me.pid, es[0].pid);
    try testing.expectEqual(@as(usize, 2), es[0].n_regions);
    try testing.expectEqual(@as(u64, 0xb1), es[0].regions[0].writer_id);
    try testing.expectEqual(@as(usize, 1), es[0].n_retired);
    try testing.expectEqual(@as(u32, 3), es[0].retired[0].appender_id);
    try testing.expectEqual(@as(usize, 0), try reg.othersAlive(testing.allocator, me.pid));
    // One region out (its `r` line with it), the other stands.
    reg.removeRegion(me.pid, "jr", 0xb1);
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
    try reg.append(me.pid, me.start_ticks, regionLine(&lb, .{ .role = .joiner, .slot = 9, .writer_id = 0xb5 }));
    {
        const fresh = try reg.entries(testing.allocator);
        defer testing.allocator.free(fresh);
        try testing.expectEqual(@as(usize, 1), fresh.len);
        try testing.expectEqual(me.start_ticks, fresh[0].start_ticks);
        try testing.expectEqual(@as(usize, 1), fresh[0].n_regions);
        try testing.expectEqual(@as(u64, 0xb5), fresh[0].regions[0].writer_id);
    }
    reg.removeRegion(me.pid, "j", 0xb5);
    const none = try reg.entries(testing.allocator);
    defer testing.allocator.free(none);
    try testing.expectEqual(@as(usize, 0), none.len);
}

test "B1 (review): a re-joined-in-place member's retired mark covers ONLY its retired incarnation's page — the successor incarnation (the same writer id) evicted later, its page Recovering, is STALE; while its page is Live it is current; the retired page alone is spared" {
    const node: u64 = 0xA;
    var e = Entry.parse(42, "77\nj 9 00000000000000b1\nr 00000000000000b1 3\n").?;
    try testing.expectEqual(Verdict.stale, judge(&e, &.{ pg(3, .recovered, node, 0xb1), pg(5, .recovering, node, 0xb1) }, node, 0, false));
    try testing.expectEqual(Verdict.current, judge(&e, &.{ pg(3, .recovered, node, 0xb1), pg(5, .live, node, 0xb1) }, node, 0, false));
    try testing.expectEqual(Verdict.retired, judge(&e, &.{pg(3, .recovered, node, 0xb1)}, node, 0, false));
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
    try reg.append(sib_pid, ticks, regionLine(&lb, .{ .role = .joiner, .slot = 9, .writer_id = sib.writerId(9) }));
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
    const fresh = registerUnderRule(ctx, &reg, &c, .{ .ctx = &pages, .read = &RacingPages.read }, .{ .ctx = undefined, .run = &registerLadderFn }) catch |e| switch (e) {
        // A refused registration takes its own key back with it.
        error.SiblingLive => {
            try testing.expect(!(try hostRegistered(&c)));
            return;
        },
        else => return e,
    };
    try testing.expect(fresh);
    // The sibling stale at the re-judge: killed before the lock is let go.
    try testing.expect(!proc.isWriterIncarnation(local, sib_pid, 9, sib.writerId(9)));
    _ = try child.wait(testing.io);
    reaped = true;
}
