//! Per-host fencing (owner ruling 2026-11-02, option C; decision 0003 §DF;
//! charter §5.5 "Fencing"): every mount of a set on one host presents ONE
//! NVMe host identity and ONE registration key, so the device fences the
//! HOST (the GPFS node model) — a preempt of the key fences every mount of
//! the set on the host, and a registration admits every one of them.
//!
//! The safety rule this module owns: NVMe keeps a registration per host
//! identifier, so a registration that admits the host afresh (the report
//! holds none of our wire host id under our key — a first mount, a fenced
//! host, a lapse) re-admits every process of the host. Before it, every
//! STALE sibling — another live incarnation of the set's mounts on this host
//! whose region a recoverer took (its page Recovering, Recovered or Free;
//! the manager superseded on appender 0) — is proven dead by the §DA
//! writer-id proof (`proc.liveWriterPid`) or killed through a pidfd and its
//! exit awaited; one that can be neither refuses the registration loud by
//! name. A CURRENT sibling (a Live page names its incarnation), one that
//! declared its incarnation retired (its latch failed: §BS's re-joiner) and
//! one still joining (no page names it) are spared: none holds a region a
//! recoverer took, so re-admitting it re-admits no stale write.
//!
//! The candidates come from the host's registry of the set's mounts
//! (`<run>/hosts/set-<key>/<pid>`, keyed by the set and the host identity:
//! a mount under another identity is another host, never a sibling); the
//! same registry shape counts the users of a shared fabric association
//! (`fabric-<key>`). Every mutation and every judgement runs under the
//! registry's flock. Linux only.

const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;
const Io = std.Io;

const log = @import("../../core/log.zig");
const knobs = @import("../../core/knobs.zig");
const checksum = @import("../../core/checksum.zig");
const journal_tail = @import("../journal_tail.zig");
const guard = @import("../guard.zig");
const claim = @import("claim.zig");
const pr = @import("pr.zig");
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

/// The set's registration key on a host: stable across remounts and
/// reboots, the same for every mount of the set under one host identity,
/// different for another identity or another set. Never 0 (0 = no key).
pub fn hostKey(hostid: []const u8, set_uuid: [16]u8) u64 {
    var h: checksum.Hasher = .init();
    h.update("squeezefs:pr-host-key");
    h.update(hostid);
    h.update(&[_]u8{0});
    h.update(&set_uuid);
    const k = h.finalLow64();
    return if (k == 0) 1 else k;
}

/// The registry key of the set's mounts under one host identity.
pub fn setRegistryKey(hostid: []const u8, set_uuid: [16]u8) u64 {
    var h: checksum.Hasher = .init();
    h.update("squeezefs:host-set");
    h.update(hostid);
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
};

pub const Role = enum { manager, joiner };

/// One region an entry's process holds (its writer id at its slot).
pub const Region = struct { role: Role, slot: u32, writer_id: u64 };

pub const MAX_REGIONS = 16; // one region per metadata volume a process writes: the set's member cap

pub const Entry = struct {
    pid: u32,
    start_ticks: u64,
    regions: [MAX_REGIONS]Region = undefined,
    n_regions: usize = 0,
    retired: [MAX_REGIONS]u64 = undefined,
    n_retired: usize = 0,

    pub fn regionSlice(self: *const Entry) []const Region {
        return self.regions[0..self.n_regions];
    }

    fn isRetired(self: *const Entry, writer_id: u64) bool {
        for (self.retired[0..self.n_retired]) |w| if (w == writer_id) return true;
        return false;
    }

    /// The entry's text: the start ticks, then `m|j <slot> <writer id>` per
    /// region, `r <writer id>` per retired incarnation.
    pub fn parse(pid: u32, text: []const u8) ?Entry {
        var lines = std.mem.tokenizeScalar(u8, text, '\n');
        const first = lines.next() orelse return null;
        var e: Entry = .{ .pid = pid, .start_ticks = std.fmt.parseInt(u64, std.mem.trim(u8, first, " "), 10) catch return null }; // proc(5)'s start ticks, decimal
        while (lines.next()) |line| {
            var f = std.mem.tokenizeScalar(u8, line, ' ');
            const tag = f.next() orelse continue;
            if (std.mem.eql(u8, tag, "r")) {
                const w = std.fmt.parseInt(u64, f.next() orelse continue, 16) catch continue; // a writer id, hex as `retiredLine` writes it
                if (e.n_retired < MAX_REGIONS) {
                    e.retired[e.n_retired] = w;
                    e.n_retired += 1;
                }
                continue;
            }
            const role: Role = if (std.mem.eql(u8, tag, "m")) .manager else if (std.mem.eql(u8, tag, "j")) .joiner else continue;
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
    lock_fd: linux.fd_t = -1,

    pub fn at(base: []const u8, kind: Kind, key: u64) Registry {
        var r: Registry = .{};
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

    /// The registry's flock (blocking: a holder's judgement is bounded by
    /// one kill's wait).
    pub fn lock(self: *Registry) RegistryError!void {
        try self.mkdirs();
        var buf: [linux.PATH_MAX]u8 = undefined;
        const p = self.pathOf(&buf, ".lock");
        const rc = linux.open(p.ptr, .{ .ACCMODE = .RDWR, .CREAT = true, .CLOEXEC = true }, 0o600);
        if (linux.errno(rc) != .SUCCESS) return error.RegistryUnavailable;
        const fd: linux.fd_t = @intCast(rc);
        while (true) {
            const lr = linux.flock(fd, std.posix.LOCK.EX);
            switch (linux.errno(lr)) {
                .SUCCESS => break,
                .INTR => continue,
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

    /// Append `line` to this process's entry (created with its start ticks).
    pub fn append(self: *const Registry, pid: u32, start_ticks: u64, line: []const u8) RegistryError!void {
        try self.mkdirs();
        var nb: [24]u8 = undefined;
        var buf: [linux.PATH_MAX]u8 = undefined;
        const p = self.pathOf(&buf, std.fmt.bufPrint(&nb, "{d}", .{pid}) catch unreachable);
        const rc = linux.open(p.ptr, .{ .ACCMODE = .WRONLY, .CREAT = true, .APPEND = true, .CLOEXEC = true }, 0o600);
        if (linux.errno(rc) != .SUCCESS) return error.RegistryUnavailable;
        const fd: linux.fd_t = @intCast(rc);
        defer _ = linux.close(fd);
        var st: linux.Statx = undefined;
        if (linux.errno(linux.statx(fd, "", linux.AT.EMPTY_PATH, .{ .SIZE = true }, &st)) != .SUCCESS) return error.RegistryUnavailable;
        var tb: [32]u8 = undefined;
        if (st.size == 0) try writeAll(fd, std.fmt.bufPrint(&tb, "{d}\n", .{start_ticks}) catch unreachable);
        try writeAll(fd, line);
    }

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

    pub fn remove(self: *const Registry, pid: u32) void {
        var nb: [24]u8 = undefined;
        var buf: [linux.PATH_MAX]u8 = undefined;
        const p = self.pathOf(&buf, std.fmt.bufPrint(&nb, "{d}", .{pid}) catch unreachable);
        _ = linux.unlink(p.ptr);
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
            const pid = std.fmt.parseInt(u32, name, 10) catch continue; // `.lock` and strays
            var buf: [linux.PATH_MAX]u8 = undefined;
            const p = self.pathOf(&buf, name);
            var tb: [4096]u8 = undefined;
            const text = proc.readSmall(p.ptr, &tb) orelse continue;
            const e = Entry.parse(pid, text) orelse continue;
            if (proc.startTicksOf(pid) != e.start_ticks) {
                _ = linux.unlink(p.ptr);
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

pub fn regionLine(buf: []u8, r: Region) []const u8 {
    return std.fmt.bufPrint(buf, "{s} {d} {x:0>16}\n", .{ if (r.role == .manager) "m" else "j", r.slot, r.writer_id }) catch unreachable;
}

pub fn retiredLine(buf: []u8, writer_id: u64) []const u8 {
    return std.fmt.bufPrint(buf, "r {x:0>16}\n", .{writer_id}) catch unreachable;
}

// ---------------------------------------------------------------------------
// The judgement (pure).
// ---------------------------------------------------------------------------

pub const Verdict = enum {
    /// A Live page names its incarnation: its region is its own.
    current,
    /// It declared the incarnation retired (its latch failed).
    retired,
    /// No page names it yet: it holds no region a recoverer took.
    joining,
    /// A recoverer took its region (or the manager role passed): it must
    /// be dead before the host is admitted.
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
        .manager => {
            if (r.slot != vol_index) return null;
            if (!taking_manager) for (pages) |p| {
                if (p.appender_id == 0 and p.state == .live and p.identity.node_token == node_token and p.identity.writer_id == r.writer_id) return .current;
            };
            if (e.isRetired(r.writer_id)) return .retired;
            return .stale;
        },
        .joiner => {
            var named = false;
            for (pages) |p| {
                if (p.appender_id == 0 or p.identity.node_token != node_token or p.identity.writer_id != r.writer_id) continue;
                if (p.state == .live) return .current;
                named = true;
            }
            if (e.isRetired(r.writer_id)) return .retired;
            return if (named) .stale else .joining;
        },
    }
}

// ---------------------------------------------------------------------------
// The rule.
// ---------------------------------------------------------------------------

/// The sibling that could be neither proven dead nor killed.
pub const Refusal = struct { pid: u32, writer_id: u64, why: []const u8 };

pub const RuleContext = struct {
    gpa: Allocator,
    env: *const std.process.Environ.Map,
    /// The volume's path (the log's name).
    path: []const u8,
    local: claim.Identity,
    node_token: u64,
    vol_index: u32,
    taking_manager: bool,
};

/// How long a SIGKILLed sibling may take to exit (an I/O it sleeps in must
/// complete first) before the registration is refused.
pub const KILL_WAIT_MS: i32 = 10_000;

/// Under the registry's lock: every live sibling judged against `pages`;
/// a stale one proven dead or killed. `error.SiblingLive` = refused (the
/// refusal logged by name, counted).
pub fn clearStaleSiblings(ctx: RuleContext, reg: *const Registry, pages: []const page.Page) (RegistryError || Allocator.Error || error{SiblingLive})!void {
    const es = try reg.entries(ctx.gpa);
    defer ctx.gpa.free(es);
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
        reg.remove(e.pid);
        log.warn("meta volume {s}: killed the stale sibling pid {d} (writer id {x:0>16}) of this set on this host before registering the host — its region was recovered, a registration would have re-admitted its writes", .{ ctx.path, e.pid, e.regionSlice()[0].writer_id });
    }
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
};

/// Under the registry's lock, before a register: true = the register
/// admits the host afresh (the rule ran and cleared every stale sibling);
/// false = the host is registered under the key (the register re-admits
/// nobody). A report that fails is judged as not registered (the rule
/// runs: never risked).
pub fn admitBeforeRegister(ctx: RuleContext, reg: *const Registry, c: *pr.Client, pages: PageSource) (RegistryError || error{ SiblingLive, OutOfMemory, PagesUnreadable })!bool {
    if (hostRegistered(c) catch false) return false;
    const ps = pages.read(pages.ctx, ctx.gpa) catch |e| return switch (e) {
        error.OutOfMemory => error.OutOfMemory,
        else => error.PagesUnreadable,
    };
    defer ctx.gpa.free(ps);
    try clearStaleSiblings(ctx, reg, ps);
    return true;
}

/// The host identity's hostid (the explicit pair → `/etc/nvme` → the
/// machine-id derivation, `initiator.resolveHostIdentity`); null = no
/// source (the mount's per-incarnation key, the rule off — logged).
pub const HostId = struct {
    buf: [64]u8 = undefined,
    len: usize = 0,

    pub fn slice(self: *const HostId) []const u8 {
        return self.buf[0..self.len];
    }
};

pub fn hostId(env: *const std.process.Environ.Map, opts: pr.IdentityOptions) ?HostId {
    return switch (initiator.resolveHostIdentity(env, opts)) {
        .identity => |id| blk: {
            var h: HostId = .{};
            const s = id.hostid();
            @memcpy(h.buf[0..s.len], s);
            h.len = s.len;
            break :blk h;
        },
        .refusal => null,
    };
}

/// What a writer's registration rides: its key (the host's, or the
/// incarnation's under the seam) and the set's registry (null under the
/// seam or with no host identity source).
pub const Plan = struct {
    key: u64,
    reg: ?Registry,
};

pub fn plan(env: *const std.process.Environ.Map, opts: pr.IdentityOptions, set_uuid: [16]u8, incarnation_key: u64, path: []const u8) Plan {
    if (perMountIdentity(env)) return .{ .key = incarnation_key, .reg = null };
    const h = hostId(env, opts) orelse {
        log.warn("meta volume {s}: no host identity source (SQUEEZEFS_HOSTNQN/HOSTID, /etc/nvme, /etc/machine-id) — the reservation key is this incarnation's and the stale-sibling rule cannot run", .{path});
        return .{ .key = incarnation_key, .reg = null };
    };
    return .{ .key = hostKey(h.slice(), set_uuid), .reg = Registry.runtime(.set, setRegistryKey(h.slice(), set_uuid)) };
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
    /// device mutation); else the rule, then `fencing.joinAsRegistrant`.
    /// This process enters the registry under the same lock. Null = no
    /// device fence here (a file, a loop device, no char node, RESCAP 0,
    /// the seam, no identity source): the join is the detection grade's.
    pub fn admit(ctx: RuleContext, set_uuid: [16]u8, opts: pr.IdentityOptions, slot: u32, writer_id: u64, pages: PageSource) error{ HostAdmissionRefused, OutOfMemory }!?*Joiner {
        if (perMountIdentity(ctx.env)) return null;
        const dev = switch (pr_linux.resolve(ctx.gpa, ctx.path)) {
            .none => return null,
            .device => |d| d,
        };
        errdefer dev.deinit();
        const p = plan(ctx.env, opts, set_uuid, 0, ctx.path);
        var reg = p.reg orelse {
            dev.deinit();
            return null;
        };
        const c = try ctx.gpa.create(pr.Client);
        errdefer ctx.gpa.destroy(c);
        c.* = try pr.Client.init(ctx.gpa, dev.face(), dev.nsid, p.key);
        errdefer c.deinit();
        const rescap = c.probeRescap() catch 0;
        if (rescap == 0) {
            c.deinit();
            ctx.gpa.destroy(c);
            dev.deinit();
            return null;
        }
        reg.lock() catch {
            log.err("meta volume {s}: the host's registry of this set ({s}) cannot be opened — refusing the join (the stale-sibling rule cannot run)", .{ ctx.path, reg.dir() });
            return error.HostAdmissionRefused;
        };
        defer reg.unlock();
        const fresh = admitBeforeRegister(ctx, &reg, c, pages) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            error.SiblingLive => return error.HostAdmissionRefused,
            error.RegistryUnavailable, error.PagesUnreadable => {
                log.err("meta volume {s}: the stale-sibling rule could not read its inputs ({s}) — refusing to register this host", .{ ctx.path, @errorName(e) });
                return error.HostAdmissionRefused;
            },
        };
        if (fresh) {
            _ = fencing.joinAsRegistrant(c, fencing.declaredRegistrantCap(ctx.env)) catch |e| {
                var buf: [256]u8 = undefined;
                var w: Io.Writer = .fixed(&buf);
                c.writeError(&w, error.DeviceStatus) catch {};
                log.err("meta volume {s}: this host's registration (key {x:0>16}) failed ({s}; {s}) — refusing the join", .{ ctx.path, c.key, @errorName(e), w.buffered() });
                return error.HostAdmissionRefused;
            };
            guard.stats.pr_host_registrations.inc();
            log.info("meta volume {s}: registered this host on the namespace (key {x:0>16}) — every mount of the set on this host rides it", .{ ctx.path, c.key });
        } else {
            guard.stats.pr_host_adoptions.inc();
            log.info("meta volume {s}: this host is registered on the namespace under the set's key {x:0>16} — adopted (no device mutation)", .{ ctx.path, c.key });
        }
        var lb: [64]u8 = undefined;
        reg.append(ctx.local.pid, ctx.local.start_ticks, regionLine(&lb, .{ .role = .joiner, .slot = slot, .writer_id = writer_id })) catch {
            log.err("meta volume {s}: this mount could not enter the host's registry of the set ({s}) — refusing the join", .{ ctx.path, reg.dir() });
            return error.HostAdmissionRefused;
        };
        const self = try ctx.gpa.create(Joiner);
        self.* = .{ .gpa = ctx.gpa, .device = dev, .client = c, .reg = reg, .slot = slot, .writer_id = writer_id };
        return self;
    }

    /// The incarnation retired (its latch failed): declared, so a sibling's
    /// registration spares this process while it re-joins.
    pub fn declareRetired(self: *Joiner) void {
        self.reg.lock() catch return;
        defer self.reg.unlock();
        var lb: [64]u8 = undefined;
        const me = proc.localIdentity();
        self.reg.append(me.pid, me.start_ticks, retiredLine(&lb, self.writer_id)) catch {};
    }

    /// Before the clean leave: out of the registry (a leaving incarnation's
    /// Free page must never read as a stale sibling's).
    pub fn depart(self: *Joiner) void {
        self.reg.lock() catch return;
        defer self.reg.unlock();
        self.reg.remove(proc.localIdentity().pid);
    }

    /// The last process of the set on this host takes the host's
    /// registration with it (zero residue); otherwise it stays the
    /// siblings'.
    pub fn deinit(self: *Joiner) void {
        if (self.reg.lock()) |_| {
            defer self.reg.unlock();
            const pid = proc.localIdentity().pid;
            self.reg.remove(pid);
            const others = self.reg.othersAlive(self.gpa, pid) catch 1;
            if (others == 0) self.client.unregister(self.client.key) catch {};
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
    var r = Entry.parse(43, "78\nj 9 00000000000000b2\nr 00000000000000b2\n").?;
    try testing.expectEqual(Verdict.retired, judge(&r, &.{pg(4, .recovered, node, 0xb2)}, node, 0, false));
    var m = Entry.parse(44, "79\nm 0 00000000000000c1\nm 1 00000000000000c2\n").?;
    try testing.expectEqual(Verdict.current, judge(&m, &.{pg(0, .live, node, 0xc1)}, node, 0, false));
    try testing.expectEqual(Verdict.stale, judge(&m, &.{pg(0, .live, node, 0xd1)}, node, 0, false));
    try testing.expectEqual(Verdict.stale, judge(&m, &.{pg(0, .live, node, 0xc1)}, node, 0, true));
    // Volume 1: only the manager region at slot 1 is judged.
    try testing.expectEqual(Verdict.current, judge(&m, &.{pg(0, .live, node, 0xc2)}, node, 1, false));
    try testing.expect(Entry.parse(1, "") == null);
    try testing.expect(Entry.parse(1, "x\n") == null);
}

test "the registry: entries append and parse; a dead process's entry is pruned by its start ticks; the flock serializes; removal" {
    var tb: [128]u8 = undefined;
    const base = try std.fmt.bufPrint(&tb, "/build/tmp/host-fence-unit-{d}", .{linux.getpid()});
    defer Io.Dir.cwd().deleteTree(testing.io, base) catch {};
    var reg = Registry.at(base, .set, 0x1234);
    reg.lock() catch return @import("../../core/testkit.zig").skip(@src(), .no_device, "/build/tmp is not writable on this host (the venue's cache volume)");
    defer reg.unlock();
    const me = proc.localIdentity();
    var lb: [64]u8 = undefined;
    try reg.append(me.pid, me.start_ticks, regionLine(&lb, .{ .role = .joiner, .slot = 9, .writer_id = 0xb1 }));
    try reg.append(me.pid, me.start_ticks, retiredLine(&lb, 0xb1));
    // A dead process (pid 1's start ticks never match 1).
    try reg.append(1, 1, regionLine(&lb, .{ .role = .manager, .slot = 0, .writer_id = 0xc1 }));
    const es = try reg.entries(testing.allocator);
    defer testing.allocator.free(es);
    try testing.expectEqual(@as(usize, 1), es.len);
    try testing.expectEqual(me.pid, es[0].pid);
    try testing.expectEqual(@as(usize, 1), es[0].n_regions);
    try testing.expectEqual(@as(u64, 0xb1), es[0].regions[0].writer_id);
    try testing.expectEqual(@as(usize, 1), es[0].n_retired);
    try testing.expectEqual(@as(usize, 0), try reg.othersAlive(testing.allocator, me.pid));
    reg.remove(me.pid);
    const none = try reg.entries(testing.allocator);
    defer testing.allocator.free(none);
    try testing.expectEqual(@as(usize, 0), none.len);
}
