//! `squeezefs nvmeof {share, unshare, list, restore, adopt, target *}`
//! (charter §8; `docs/porting/ops_nvmet_keys.md` §1.7, §2.5–§2.8): every
//! verb RECONCILES the ledger against the live configfs walk (law 4) and
//! never trusts configfs bookkeeping; `share` and `unshare` write their
//! intent BEFORE the first kernel mutation (law 6); the duplicate guard's
//! refusal IS the runbook (the holder's classification and the exits
//! `unshare` / the manual configfs steps / `adopt`); `restore` is
//! idempotent per record and never clobbers a mismatching identity;
//! `adopt` writes ONLY the ledger with a TOCTOU re-verify. Linux only.

const std = @import("std");
const testkit = @import("../../core/testkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const Allocator = std.mem.Allocator;
const Io = std.Io;

const checksum = @import("../../core/checksum.zig");
const clock = @import("../../core/clock.zig");
const knobs = @import("../../core/knobs.zig");
const log = @import("../../core/log.zig");
const uring_fs = @import("../../io/uring_fs.zig");
const align_probe = @import("../../io/align_probe.zig");
const configfs = @import("configfs.zig");
const ledger = @import("ledger.zig");
const initiator = @import("initiator.zig");
const stats = @import("stats.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("ops/nvmet/verbs is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const Listener = configfs.Listener;
pub const Record = ledger.Record;

/// What every verb runs with: the configfs face (the real tree or the
/// fake), the ledger store, the port-id base, the writers.
pub const Ctx = struct {
    gpa: Allocator,
    io: Io,
    fs: configfs.Fs,
    store: *ledger.Store,
    port_base: u16,
    stdout: *Io.Writer,
    stderr: *Io.Writer,
    /// The source of fresh namespace uuids (`Io.randomSecure`).
    random: *const fn (ctx: *Ctx, out: *[16]u8) bool = randomIo,

    fn randomIo(self: *Ctx, out: *[16]u8) bool {
        self.io.randomSecure(out) catch return false;
        return true;
    }

    fn mintUuid(self: *Ctx) ?[16]u8 {
        var u: [16]u8 = undefined;
        if (!self.random(self, &u)) return null;
        u[6] = (u[6] & 0x0f) | 0x40;
        u[8] = (u[8] & 0x3f) | 0x80;
        return u;
    }
};

/// The port-id base: the knob, else the default.
pub fn portBase(env: *const std.process.Environ.Map) u16 {
    return knobs.optIntKnob("SQUEEZEFS_NVMET_PORT_ID_BASE", u16, env) orelse configfs.PORT_ID_BASE_DEFAULT;
}

pub const Outcome = enum(u8) { ok = 0, failed = 1 };

// ---------------------------------------------------------------------------
// share
// ---------------------------------------------------------------------------

pub const ShareOptions = struct {
    backing_path: []const u8,
    /// Null = `OWNERSHIP_NQN_PREFIX ‖ hash64(canonical backing)`.
    subnqn: ?[]const u8 = null,
    trtype: configfs.Trtype = .tcp,
    /// `--ip` (tcp): ≥ 1; forbidden on loop.
    ips: []const []const u8 = &.{},
    port: u16 = configfs.DEFAULT_TRSVCID,
    allow_hosts: []const []const u8 = &.{},
    /// `--create-size BYTES`: create the backing (sparse) when absent.
    create_size: ?u64 = null,
    /// `--ns-uuid`: the identity to re-present (restore seeding).
    ns_uuid: ?[16]u8 = null,
};

fn fail(ctx: *Ctx, comptime fmt: []const u8, args: anytype) Outcome {
    ctx.stderr.print("squeezefs nvmeof: " ++ fmt ++ "\n", args) catch {};
    ctx.stderr.flush() catch {};
    return .failed;
}

/// The default share NQN: classification only, never ownership.
pub fn defaultSubnqn(buf: *[128]u8, canonical: []const u8) []const u8 {
    return std.fmt.bufPrint(buf, "{s}{x:0>16}", .{ configfs.OWNERSHIP_NQN_PREFIX, checksum.hash64(canonical) }) catch unreachable;
}

fn preflightFor(ctx: *Ctx, verb: []const u8) ?Outcome {
    var e: ?linux.E = null;
    const r = configfs.preflight(&e);
    if (r == .ready) return null;
    var buf: [512]u8 = undefined;
    var w: Io.Writer = .fixed(&buf);
    configfs.writeReadinessRefusal(&w, r, e) catch {};
    return fail(ctx, "{s}: {s}", .{ verb, w.buffered() });
}

const Holder = enum { managed, pending, removing, foreign };

fn classifyHolder(l: *const ledger.Ledger, subnqn: []const u8) Holder {
    const r = l.find(subnqn) orelse return .foreign;
    return switch (r.state) {
        .active => .managed,
        .pending => .pending,
        .removing => .removing,
    };
}

fn writeManualSteps(w: *Io.Writer, subnqn: []const u8) Io.Writer.Error!void {
    try w.print("manual removal (configfs): unlink {s}/ports/*/subsystems/{s}; echo 0 > {s}/subsystems/{s}/namespaces/1/enable; rmdir {s}/subsystems/{s}/namespaces/1; rmdir {s}/subsystems/{s}", .{ configfs.NVMET_ROOT, subnqn, configfs.NVMET_ROOT, subnqn, configfs.NVMET_ROOT, subnqn, configfs.NVMET_ROOT, subnqn });
}

fn writeHolderExits(w: *Io.Writer, holder: Holder, subnqn: []const u8) Io.Writer.Error!void {
    switch (holder) {
        .managed => try w.print("it is a MANAGED share: `squeezefs nvmeof unshare {s}` releases it", .{subnqn}),
        .pending => try w.writeAll("it is a PENDING share (an interrupted `share`): `squeezefs nvmeof restore` finalizes or garbage-collects it"),
        .removing => try w.writeAll("it is a REMOVING share (an interrupted `unshare`): `squeezefs nvmeof restore` resumes the teardown"),
        .foreign => {
            try w.print("it is FOREIGN (not in the ledger): `squeezefs nvmeof adopt {s}` to manage it, or ", .{subnqn});
            try writeManualSteps(w, subnqn);
        },
    }
}

const LedgeredSet = struct {
    l: *const ledger.Ledger,
    fn isLedgered(ctx: *const anyopaque, nqn: []const u8) bool {
        const self: *const LedgeredSet = @ptrCast(@alignCast(ctx));
        return self.l.isLedgered(nqn);
    }
    fn ownership(self: *const LedgeredSet) configfs.Ownership {
        return .{ .ctx = @ptrCast(self), .isLedgered = isLedgered };
    }
};

/// Create the backing sparse (`--create-size`) or refuse an absent one.
fn ensureBacking(ctx: *Ctx, path: []const u8, create_size: ?u64) Outcome {
    var zb: [linux.PATH_MAX]u8 = undefined;
    const z = align_probe.pathZ(&zb, path) catch return fail(ctx, "share: backing path too long", .{});
    var st: linux.Statx = undefined;
    if (linux.errno(linux.statx(linux.AT.FDCWD, z, 0, .{ .TYPE = true, .SIZE = true }, &st)) == .SUCCESS) {
        if (!linux.S.ISREG(st.mode) and !linux.S.ISBLK(st.mode)) return fail(ctx, "share: {s} is neither a regular file nor a block device — nvmet serves only those", .{path});
        if (linux.S.ISREG(st.mode) and st.size == 0) return fail(ctx, "share: {s} is an empty file — nvmet cannot serve a zero-length namespace; `truncate -s <size>` it (or pass --create-size on a fresh path)", .{path});
        return .ok;
    }
    const size = create_size orelse return fail(ctx, "share: backing {s} does not exist — create it first (`truncate -s 1G {s}`) or pass --create-size <bytes> to create it sparse here", .{ path, path });
    const rc = linux.open(z, .{ .ACCMODE = .RDWR, .CREAT = true, .EXCL = true, .CLOEXEC = true }, 0o600);
    if (linux.errno(rc) != .SUCCESS) return fail(ctx, "share: cannot create {s} ({t})", .{ path, linux.errno(rc) });
    const fd: linux.fd_t = @intCast(rc);
    defer _ = linux.close(fd);
    if (linux.errno(linux.ftruncate(fd, @intCast(size))) != .SUCCESS) return fail(ctx, "share: cannot size {s} to {d} bytes", .{ path, size });
    return .ok;
}

pub fn share(ctx: *Ctx, o: ShareOptions) Outcome {
    const gpa = ctx.gpa;
    // Grammar before any side effect.
    if (o.subnqn) |n| configfs.validateNqn(n) catch |e| {
        var buf: [512]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        configfs.writeNqnRefusal(&w, e, n) catch {};
        return fail(ctx, "share: --subnqn: {s}", .{w.buffered()});
    };
    for (o.allow_hosts) |h| configfs.validateNqn(h) catch |e| {
        var buf: [512]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        configfs.writeNqnRefusal(&w, e, h) catch {};
        return fail(ctx, "share: --allow-host: {s}", .{w.buffered()});
    };
    var listeners_buf: [16]Listener = undefined;
    var n_listeners: usize = 0;
    switch (o.trtype) {
        .tcp => {
            if (o.ips.len == 0) return fail(ctx, "share: --transport tcp needs at least one --ip <address> (the listener); use --transport loop for a same-host namespace", .{});
            if (o.ips.len > listeners_buf.len) return fail(ctx, "share: more than {d} --ip listeners", .{listeners_buf.len});
            for (o.ips) |ip| {
                const l = Listener.tcp(ip, o.port) catch return fail(ctx, "share: --ip '{s}' is not an IPv4/IPv6 address", .{ip});
                listeners_buf[n_listeners] = l;
                n_listeners += 1;
            }
        },
        .loop => {
            if (o.ips.len != 0) return fail(ctx, "share: --transport loop takes no --ip (a loop port carries no address)", .{});
            listeners_buf[0] = Listener.loop;
            n_listeners = 1;
        },
    }
    const listeners = listeners_buf[0..n_listeners];
    if (ensureBacking(ctx, o.backing_path, o.create_size) == .failed) return .failed;
    const canonical = (uring_fs.canonicalize(gpa, o.backing_path) catch return fail(ctx, "out of memory", .{})) orelse return fail(ctx, "share: {s} does not resolve", .{o.backing_path});
    defer gpa.free(canonical);
    var nqn_buf: [128]u8 = undefined;
    const subnqn = o.subnqn orelse defaultSubnqn(&nqn_buf, canonical);
    if (preflightFor(ctx, "share")) |r| return r;

    // The live walk + the ledger: the duplicate guard BEFORE any side effect.
    var walk = configfs.walkLive(ctx.fs, gpa) catch |e| return fail(ctx, "share: cannot walk {s}: {s}", .{ configfs.NVMET_ROOT, @errorName(e) });
    defer walk.deinit(gpa);
    var l = ctx.store.load() catch |e| return fail(ctx, "share: cannot load the ledger: {s}", .{@errorName(e)});
    defer l.deinit(gpa);
    for (walk.shares) |*s| {
        if (!s.materialized()) continue;
        var buf: [1024]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        if (std.mem.eql(u8, s.subnqn, subnqn)) {
            writeHolderExits(&w, classifyHolder(&l, s.subnqn), s.subnqn) catch {};
            return fail(ctx, "share: {s} is already live on this host's target — {s}", .{ subnqn, w.buffered() });
        }
        if (std.mem.eql(u8, s.device_path, o.backing_path) or std.mem.eql(u8, s.device_path, canonical)) {
            writeHolderExits(&w, classifyHolder(&l, s.subnqn), s.subnqn) catch {};
            return fail(ctx, "share: the same backing {s} is already served by {s} — the same backing must never be double-served; {s}", .{ canonical, s.subnqn, w.buffered() });
        }
    }
    if (l.find(subnqn)) |r| {
        var buf: [1024]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        writeHolderExits(&w, classifyHolder(&l, subnqn), subnqn) catch {};
        _ = r;
        return fail(ctx, "share: {s} is in the ledger — {s}", .{ subnqn, w.buffered() });
    }
    if (l.findByBacking(canonical)) |r| {
        var buf: [1024]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        writeHolderExits(&w, classifyHolder(&l, r.subnqn), r.subnqn) catch {};
        return fail(ctx, "share: the backing {s} is recorded under {s} — {s}", .{ canonical, r.subnqn, w.buffered() });
    }

    // Port ids over the snapshot, BEFORE the pending record (the intent
    // claims them). One id per distinct endpoint.
    const owned: LedgeredSet = .{ .l = &l };
    for (listeners, 0..) |*li, i| {
        var reused = false;
        for (listeners[0..i]) |prev| if (prev.sameEndpoint(li.*)) {
            li.port_id = prev.port_id;
            reused = true;
        };
        if (reused) continue;
        li.port_id = configfs.allocatePortId(ctx.port_base, li.*, walk.ports, owned.ownership()) catch {
            return fail(ctx, "share: every port id in [{d}, {d}] is taken by a foreign or incompatible nvmet port — relocate the range with SQUEEZEFS_NVMET_PORT_ID_BASE (1..={d})", .{ ctx.port_base, ctx.port_base + configfs.PORT_ID_RANGE - 1, configfs.PORT_ID_BASE_MAX });
        };
    }
    const ns_uuid = o.ns_uuid orelse (ctx.mintUuid() orelse return fail(ctx, "share: getrandom failed (the namespace uuid); refusing a weaker identity", .{}));

    // The record (pending) BEFORE the first configfs mutation (law 6).
    var hosts = gpa.alloc([]u8, o.allow_hosts.len) catch return fail(ctx, "out of memory", .{});
    var nh: usize = 0;
    defer {
        for (hosts[0..nh]) |h| gpa.free(h);
        gpa.free(hosts);
    }
    for (o.allow_hosts) |h| {
        hosts[nh] = gpa.dupe(u8, h) catch return fail(ctx, "out of memory", .{});
        nh += 1;
    }
    const rec: Record = .{
        .state = .pending,
        .trtype = o.trtype,
        .adopted = false,
        .allow_any_host = o.allow_hosts.len == 0,
        .created_s = @intCast(clock.realtimeCoarseS()),
        .adopted_s = 0,
        .adopt_class = .none,
        .ns_uuid = ns_uuid,
        .subnqn = @constCast(subnqn),
        .backing_path = @constCast(o.backing_path),
        .backing_canonical = canonical,
        .listeners = listeners,
        .hosts = hosts,
    };
    const Begin = struct { gpa: Allocator, rec: *const Record };
    ctx.store.mutate(Begin{ .gpa = gpa, .rec = &rec }, struct {
        fn f(c: Begin, lg: *ledger.Ledger) anyerror!void {
            try ledger.beginShare(c.gpa, lg, c.rec);
        }
    }.f) catch |e| switch (e) {
        error.DuplicateSubnqn => return fail(ctx, "share: {s} was recorded by a concurrent share — refusing (the ledger is the authority)", .{subnqn}),
        error.DuplicateBacking => return fail(ctx, "share: the backing {s} was recorded by a concurrent share — refusing", .{canonical}),
        else => return fail(ctx, "share: the pending intent could not be written: {s} (nothing was mutated)", .{@errorName(e)}),
    };

    // The recipe.
    const applied = configfs.apply(ctx.fs, .{ .subnqn = subnqn, .backing_path = canonical, .ns_uuid = ns_uuid, .allow_hosts = o.allow_hosts, .listeners = listeners }) catch |e| {
        return fail(ctx, "share: {s} failed part-way ({s}) — the PENDING record claims what exists; `squeezefs nvmeof restore` finalizes or garbage-collects it, `squeezefs nvmeof list` shows it", .{ subnqn, @errorName(e) });
    };
    const Fin = struct { subnqn: []const u8 };
    ctx.store.mutate(Fin{ .subnqn = subnqn }, struct {
        fn f(c: Fin, lg: *ledger.Ledger) anyerror!void {
            try ledger.finalizeShare(lg, c.subnqn);
        }
    }.f) catch |e| return fail(ctx, "share: {s} is live but its record could not flip active ({s}) — `squeezefs nvmeof restore` finalizes the pending intent", .{ subnqn, @errorName(e) });

    var ub: [36]u8 = undefined;
    ctx.stdout.print("shared {s}\n  backing {s}\n  ns_uuid {s}\n  pr {s}\n  hosts {s}\n", .{ subnqn, canonical, configfs.uuidText(&ub, ns_uuid), if (applied.resv_enabled) "resv_enable=1 (NVMe Persistent Reservations armed)" else "NONE (this kernel's nvmet has no resv_enable — detection-grade guard)", if (o.allow_hosts.len == 0) "allow-any (the trusted-fabric posture)" else "allow-list" }) catch {};
    for (listeners) |li| {
        var ab: [64]u8 = undefined;
        if (li.trtype == .loop) {
            ctx.stdout.print("  listener loop (port id {d}): connect with `squeezefs nvmeof connect --transport loop {s}`\n", .{ li.port_id, subnqn }) catch {};
        } else {
            ctx.stdout.print("  listener tcp {s}:{d} (port id {d}): connect with `squeezefs nvmeof connect --ip {s} --port {d} {s}`\n", .{ li.traddrText(&ab), li.trsvcid, li.port_id, li.traddrText(&ab), li.trsvcid, subnqn }) catch {};
        }
    }
    ctx.stdout.flush() catch {};
    return .ok;
}

// ---------------------------------------------------------------------------
// unshare
// ---------------------------------------------------------------------------

const HostsElsewhere = struct {
    l: *const ledger.Ledger,
    except: []const u8,
    fn named(ctx: *const anyopaque, host: []const u8) bool {
        const self: *const HostsElsewhere = @ptrCast(@alignCast(ctx));
        return self.l.hostNamedElsewhere(host, self.except);
    }
};

/// The teardown by the RECORD (its ids, its hosts), tolerant of vanished
/// objects; the ledger decides what `hosts/<h>` survive.
fn teardownRecord(ctx: *Ctx, l: *const ledger.Ledger, r: *const Record) !configfs.Torn {
    var ids_buf: [ledger.MAX_LISTENERS]u16 = undefined;
    const he: HostsElsewhere = .{ .l = l, .except = r.subnqn };
    return configfs.teardown(ctx.fs, ctx.gpa, .{ .subnqn = r.subnqn, .port_ids = r.portIds(&ids_buf), .allow_hosts = r.hostsConst(), .host_named_elsewhere = HostsElsewhere.named, .ctx = @ptrCast(&he) });
}

pub fn unshare(ctx: *Ctx, subnqn: []const u8) Outcome {
    const gpa = ctx.gpa;
    var l = ctx.store.load() catch |e| return fail(ctx, "unshare: cannot load the ledger: {s}", .{@errorName(e)});
    defer l.deinit(gpa);
    const r = l.find(subnqn) orelse {
        var buf: [1024]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        writeManualSteps(&w, subnqn) catch {};
        return fail(ctx, "unshare: {s} is not in the ledger — SqueezeFS never tears down target objects it did not record. `squeezefs nvmeof adopt {s}` first, or {s}", .{ subnqn, subnqn, w.buffered() });
    };
    const Mark = struct { subnqn: []const u8 };
    ctx.store.mutate(Mark{ .subnqn = subnqn }, struct {
        fn f(c: Mark, lg: *ledger.Ledger) anyerror!void {
            try ledger.markRemoving(lg, c.subnqn);
        }
    }.f) catch |e| return fail(ctx, "unshare: the removing intent could not be written: {s} (nothing was torn down)", .{@errorName(e)});
    // Unshare tolerates an absent tree (a vanished object is a verified no-op).
    const torn = if (configfs.nvmetPresent()) teardownRecord(ctx, &l, r) catch |e| {
        return fail(ctx, "unshare: teardown of {s} failed part-way ({s}) — the record stays REMOVING; `squeezefs nvmeof restore` resumes it", .{ subnqn, @errorName(e) });
    } else configfs.Torn{ .subsystem_absent = true, .ports_removed = 0, .hosts_removed = 0 };
    const Del = struct { gpa: Allocator, subnqn: []const u8 };
    ctx.store.mutate(Del{ .gpa = gpa, .subnqn = subnqn }, struct {
        fn f(c: Del, lg: *ledger.Ledger) anyerror!void {
            try ledger.delete(c.gpa, lg, c.subnqn);
        }
    }.f) catch |e| return fail(ctx, "unshare: {s} is torn down but its record could not be deleted ({s}) — `squeezefs nvmeof restore` resumes", .{ subnqn, @errorName(e) });
    ctx.stdout.print("unshared {s}{s} (ports removed {d}, hosts removed {d})\n", .{ subnqn, if (torn.subsystem_absent) " (the subsystem was already gone — a verified no-op)" else "", torn.ports_removed, torn.hosts_removed }) catch {};
    ctx.stdout.flush() catch {};
    return .ok;
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

pub const Classification = enum {
    managed,
    down,
    pending,
    removing,

    pub fn text(self: Classification) []const u8 {
        return switch (self) {
            .managed => "managed",
            .down => "down — restore candidate",
            .pending => "interrupted share; `nvmeof restore` finalizes or garbage-collects it",
            .removing => "interrupted unshare; `nvmeof restore` resumes the teardown",
        };
    }
};

pub fn classify(r: *const Record, live: bool) Classification {
    return switch (r.state) {
        .pending => .pending,
        .removing => .removing,
        .active => if (live) .managed else .down,
    };
}

fn liveOf(walk: *const configfs.Walk, subnqn: []const u8) bool {
    const s = walk.find(subnqn) orelse return false;
    return s.materialized();
}

pub fn list(ctx: *Ctx, json: bool) Outcome {
    const gpa = ctx.gpa;
    var l = ctx.store.load() catch |e| {
        var buf: [512]u8 = undefined;
        var w: Io.Writer = .fixed(&buf);
        switch (e) {
            error.NewerBinary, error.BadMagic, error.ChecksumMismatch, error.Truncated, error.BadRecord, error.UnknownFlag, error.ReservedNotZero, error.BadLength, error.BadVersion, error.TrailingBytes, error.BadState, error.BadTrtype, error.BadAdrfam, error.BadAdoptClass => ledger.writeDecodeRefusal(&w, @as(ledger.DecodeError, @errorCast(e)), ctx.store.filePath()) catch {},
            else => w.print("{s}", .{@errorName(e)}) catch {},
        }
        return fail(ctx, "list: {s}", .{w.buffered()});
    };
    defer l.deinit(gpa);
    // List tolerates an absent tree.
    var walk: configfs.Walk = if (configfs.nvmetPresent()) (configfs.walkLive(ctx.fs, gpa) catch |e| return fail(ctx, "list: cannot walk {s}: {s}", .{ configfs.NVMET_ROOT, @errorName(e) })) else .{ .shares = &.{}, .ports = .{ .entries = &.{} } };
    defer walk.deinit(gpa);
    const ctrls = initiator.listControllers(gpa, initiator.SYSFS_ROOT) catch return fail(ctx, "out of memory", .{});
    defer gpa.free(ctrls);
    const w = ctx.stdout;
    if (json) {
        w.print("{{\"ledger_generation\":{d},\"ledger_path\":\"{s}\",\"shares\":[", .{ l.generation, ctx.store.filePath() }) catch {};
        for (l.records.items, 0..) |*r, i| {
            if (i != 0) w.writeByte(',') catch {};
            const live = liveOf(&walk, r.subnqn);
            w.writeAll("{\"record\":") catch {};
            r.writeJson(w) catch {};
            w.print(",\"live\":{},\"classification\":\"{s}\"}}", .{ live, @tagName(classify(r, live)) }) catch {};
        }
        w.writeAll("],\"foreign_live\":[") catch {};
        var first = true;
        for (walk.shares) |*s| {
            if (l.isLedgered(s.subnqn) or !s.materialized()) continue;
            if (!first) w.writeByte(',') catch {};
            first = false;
            writeLiveJson(w, s) catch {};
        }
        w.writeAll("],\"connected_fabric_disks\":[") catch {};
        first = true;
        for (ctrls) |*c| {
            if (!c.isFabric()) continue;
            if (!first) w.writeByte(',') catch {};
            first = false;
            const ns = initiator.findNamespaceUnderController(gpa, initiator.SYSFS_ROOT, c.instance) catch null;
            w.print("{{\"controller\":\"{s}\",\"device\":", .{c.nameOf()}) catch {};
            if (ns) |n| w.print("\"/dev/{s}\"", .{n.blockName()}) catch {} else w.writeAll("null") catch {};
            w.print(",\"subnqn\":\"{s}\",\"transport\":\"{s}\",\"state\":\"{s}\",\"address\":\"{s}\",\"hostnqn\":\"{s}\"}}", .{ c.subsysnqnOf(), c.transportOf(), c.stateOf(), c.addressOf(), c.hostnqnOf() }) catch {};
        }
        w.writeAll("]}\n") catch {};
        w.flush() catch {};
        return .ok;
    }
    w.print("share ledger {s} (generation {d}, {d} record(s))\n", .{ ctx.store.filePath(), l.generation, l.records.items.len }) catch {};
    for (l.records.items) |*r| {
        const live = liveOf(&walk, r.subnqn);
        var ub: [36]u8 = undefined;
        w.print("  {s}\n    state {s}; {s}; {s}\n    backing {s}; ns_uuid {s}; {s}", .{ r.subnqn, @tagName(r.state), if (live) "live" else "not live", classify(r, live).text(), r.backing_canonical, configfs.uuidText(&ub, r.ns_uuid), if (r.allow_any_host) "allow-any" else "allow-list" }) catch {};
        if (r.adopted) w.print("; adopted ({s})", .{@tagName(r.adopt_class)}) catch {};
        w.writeAll("\n    listeners:") catch {};
        for (r.listeners) |li| {
            var ab: [64]u8 = undefined;
            if (li.trtype == .loop) w.print(" loop#{d}", .{li.port_id}) catch {} else w.print(" tcp {s}:{d}#{d}", .{ li.traddrText(&ab), li.trsvcid, li.port_id }) catch {};
        }
        w.writeAll("\n") catch {};
    }
    var any_foreign = false;
    for (walk.shares) |*s| {
        if (l.isLedgered(s.subnqn) or !s.materialized()) continue;
        if (!any_foreign) w.writeAll("foreign live (not in the ledger; never touched):\n") catch {};
        any_foreign = true;
        var ub: [36]u8 = undefined;
        w.print("  {s}: device_path {s}; uuid {s}; {s}; {d} listener(s)\n", .{ s.subnqn, s.device_path, if (s.device_uuid) |u| configfs.uuidText(&ub, u) else "(none)", if (s.enabled) "enabled" else "disabled", s.listeners.len }) catch {};
    }
    var any_ctrl = false;
    for (ctrls) |*c| {
        if (!c.isFabric()) continue;
        if (!any_ctrl) w.writeAll("connected fabric disks:\n") catch {};
        any_ctrl = true;
        const ns = initiator.findNamespaceUnderController(gpa, initiator.SYSFS_ROOT, c.instance) catch null;
        w.print("  {s}: {s} {s} {s} {s} hostnqn {s}\n", .{ c.nameOf(), if (ns) |n| n.blockName() else "(no namespace)", c.subsysnqnOf(), c.transportOf(), c.stateOf(), c.hostnqnOf() }) catch {};
    }
    w.flush() catch {};
    return .ok;
}

fn writeLiveJson(w: *Io.Writer, s: *const configfs.LiveShare) Io.Writer.Error!void {
    try w.print("{{\"subnqn\":\"{s}\",\"device_path\":\"{s}\",\"ns_uuid\":", .{ s.subnqn, s.device_path });
    if (s.device_uuid) |u| {
        try w.writeByte('"');
        try configfs.writeUuid(w, u);
        try w.writeByte('"');
    } else try w.writeAll("null");
    try w.print(",\"enabled\":{},\"allow_any_host\":{},\"nsids\":[", .{ s.enabled, s.allow_any_host });
    for (s.nsids, 0..) |n, i| try w.print("{s}{d}", .{ if (i == 0) "" else ",", n });
    try w.writeAll("],\"listeners\":[");
    for (s.listeners, 0..) |li, i| {
        if (i != 0) try w.writeByte(',');
        try w.print("{{\"port_id\":{d},\"trtype\":\"{s}\",\"traddr\":\"{s}\",\"trsvcid\":\"{s}\"}}", .{ li.port_id, if (li.trtype) |t| t.name() else "", li.traddr, li.trsvcid });
    }
    try w.writeAll("]}");
}

// ---------------------------------------------------------------------------
// restore
// ---------------------------------------------------------------------------

pub const RestoreOutcome = enum {
    teardown_resumed,
    finalized_pending,
    garbage_collected_pending,
    verified_noop,
    restored,
    failed,

    pub fn text(self: RestoreOutcome) []const u8 {
        return switch (self) {
            .teardown_resumed => "teardown resumed (removing intent completed; record deleted)",
            .finalized_pending => "pending intent finalized (the share was live and matched its record)",
            .garbage_collected_pending => "garbage-collected (the interrupted share never completed and never returned success; its objects were removed)",
            .verified_noop => "already live — verified no-op",
            .restored => "restored (re-established with the recorded identity and port ids)",
            .failed => "FAILED",
        };
    }
};

/// `live_matches_record` (N:861-890): the backing serves and the uuid
/// equals the recorded one; a live namespace WITHOUT a uuid while the
/// record carries one is a mismatch.
pub fn liveMatchesRecord(s: *const configfs.LiveShare, r: *const Record) bool {
    if (!std.mem.eql(u8, s.device_path, r.backing_path) and !std.mem.eql(u8, s.device_path, r.backing_canonical)) return false;
    const u = s.device_uuid orelse return false;
    return std.mem.eql(u8, &u, &r.ns_uuid);
}

fn applyRecord(ctx: *Ctx, r: *const Record) configfs.ApplyError!configfs.Applied {
    return configfs.apply(ctx.fs, .{ .subnqn = r.subnqn, .backing_path = r.backing_canonical, .ns_uuid = r.ns_uuid, .allow_hosts = r.hostsConst(), .listeners = r.listeners });
}

pub fn restore(ctx: *Ctx) Outcome {
    const gpa = ctx.gpa;
    if (preflightFor(ctx, "restore")) |r| return r;
    var l = ctx.store.load() catch |e| return fail(ctx, "restore: cannot load the ledger: {s}", .{@errorName(e)});
    defer l.deinit(gpa);
    var any_failed = false;
    // Per-record errors are COLLECTED, never short-circuited.
    for (l.records.items) |*r| {
        var walk = configfs.walkLive(ctx.fs, gpa) catch |e| return fail(ctx, "restore: cannot walk {s}: {s}", .{ configfs.NVMET_ROOT, @errorName(e) });
        defer walk.deinit(gpa);
        const live = walk.find(r.subnqn);
        const materialized = if (live) |s| s.materialized() else false;
        var outcome: RestoreOutcome = .failed;
        var detail: [512]u8 = undefined;
        var dw: Io.Writer = .fixed(&detail);
        switch (r.state) {
            .removing => {
                if (teardownRecord(ctx, &l, r)) |_| {
                    deleteRecord(ctx, r.subnqn) catch |e| dw.print("record delete failed: {s}", .{@errorName(e)}) catch {};
                    if (dw.buffered().len == 0) outcome = .teardown_resumed;
                } else |e| dw.print("teardown failed: {s}", .{@errorName(e)}) catch {};
            },
            .pending => {
                if (materialized and liveMatchesRecord(live.?, r)) {
                    finalizeRecord(ctx, r.subnqn) catch |e| dw.print("finalize failed: {s}", .{@errorName(e)}) catch {};
                    if (dw.buffered().len == 0) outcome = .finalized_pending;
                } else if (materialized) {
                    dw.print("the live namespace's backing/uuid DO NOT match the pending record — refusing to finalize or clobber; resolve manually", .{}) catch {};
                } else {
                    if (teardownRecord(ctx, &l, r)) |_| {
                        deleteRecord(ctx, r.subnqn) catch |e| dw.print("record delete failed: {s}", .{@errorName(e)}) catch {};
                        if (dw.buffered().len == 0) outcome = .garbage_collected_pending;
                    } else |e| dw.print("garbage collection failed: {s}", .{@errorName(e)}) catch {};
                }
            },
            .active => {
                if (materialized and liveMatchesRecord(live.?, r)) {
                    outcome = .verified_noop;
                } else if (materialized) {
                    dw.print("exists live but MISMATCHES its record (backing or uuid) — never clobbered; resolve manually", .{}) catch {};
                } else {
                    if (applyRecord(ctx, r)) |_| {
                        outcome = .restored;
                    } else |e| dw.print("re-establish failed: {s}", .{@errorName(e)}) catch {};
                }
            },
        }
        switch (outcome) {
            .restored => stats.nvmet_restore_restored.inc(),
            .verified_noop => stats.nvmet_restore_noops.inc(),
            .failed => {
                stats.nvmet_restore_failures.inc();
                any_failed = true;
            },
            else => {},
        }
        if (outcome == .failed) {
            ctx.stdout.print("{s}: FAILED — {s}\n", .{ r.subnqn, dw.buffered() }) catch {};
        } else {
            ctx.stdout.print("{s}: {s}\n", .{ r.subnqn, outcome.text() }) catch {};
            if (outcome == .garbage_collected_pending) log.warn("nvmeof restore: {s}: the interrupted share never completed and never returned success — its partial objects were removed", .{r.subnqn});
        }
    }
    if (l.records.items.len == 0) ctx.stdout.writeAll("nothing to restore (the ledger is empty)\n") catch {};
    ctx.stdout.flush() catch {};
    // A systemd oneshot goes red over any unserved share.
    return if (any_failed) .failed else .ok;
}

fn deleteRecord(ctx: *Ctx, subnqn: []const u8) anyerror!void {
    const Del = struct { gpa: Allocator, subnqn: []const u8 };
    try ctx.store.mutate(Del{ .gpa = ctx.gpa, .subnqn = subnqn }, struct {
        fn f(c: Del, lg: *ledger.Ledger) anyerror!void {
            try ledger.delete(c.gpa, lg, c.subnqn);
        }
    }.f);
}

fn finalizeRecord(ctx: *Ctx, subnqn: []const u8) anyerror!void {
    const Fin = struct { subnqn: []const u8 };
    try ctx.store.mutate(Fin{ .subnqn = subnqn }, struct {
        fn f(c: Fin, lg: *ledger.Ledger) anyerror!void {
            try ledger.finalizeShare(lg, c.subnqn);
        }
    }.f);
}

// ---------------------------------------------------------------------------
// adopt
// ---------------------------------------------------------------------------

pub const AdoptRefusal = enum {
    adopt_not_live,
    adopt_already_ledgered,
    adopt_backing_duplicated,
    adopt_harness_owned,
    adopt_shape_unsupported,
    adopt_toctou_drift,
};

fn adoptFail(ctx: *Ctx, token: AdoptRefusal, comptime fmt: []const u8, args: anytype) Outcome {
    ctx.stderr.print("squeezefs nvmeof adopt: [{s}] " ++ fmt ++ "\n", .{@tagName(token)} ++ args) catch {};
    ctx.stderr.flush() catch {};
    return .failed;
}

/// The candidate record off a live share (`build_adopt_record`
/// M:827-893): the live uuid (null ⇒ a loud note), the live listeners
/// with their ids AS FOUND (out-of-range ids recorded as-is), the live
/// allow-list, the node AS the backing.
fn buildCandidate(ctx: *Ctx, s: *const configfs.LiveShare, canonical: []const u8) !Record {
    const gpa = ctx.gpa;
    var trtype: ?configfs.Trtype = null;
    var listeners = try gpa.alloc(Listener, s.listeners.len);
    errdefer gpa.free(listeners);
    for (s.listeners, 0..) |li, i| {
        const t = li.trtype orelse return error.ShapeUnsupported;
        if (trtype != null and trtype.? != t) return error.ShapeUnsupported;
        trtype = t;
        if (t == .loop) {
            listeners[i] = Listener.loop;
        } else {
            const port = std.fmt.parseInt(u16, li.trsvcid, 10) catch return error.ShapeUnsupported;
            listeners[i] = Listener.tcp(li.traddr, port) catch return error.ShapeUnsupported;
        }
        listeners[i].port_id = li.port_id;
    }
    const hosts = try gpa.alloc([]u8, s.allow_hosts.len);
    var nh: usize = 0;
    errdefer {
        for (hosts[0..nh]) |h| gpa.free(h);
        gpa.free(hosts);
    }
    for (s.allow_hosts) |h| {
        hosts[nh] = try gpa.dupe(u8, h);
        nh += 1;
    }
    const subnqn = try gpa.dupe(u8, s.subnqn);
    errdefer gpa.free(subnqn);
    const backing = try gpa.dupe(u8, s.device_path);
    errdefer gpa.free(backing);
    const canon = try gpa.dupe(u8, canonical);
    errdefer gpa.free(canon);
    const now: i64 = @intCast(clock.realtimeCoarseS());
    return .{
        .state = .pending,
        .trtype = trtype orelse return error.ShapeUnsupported,
        .adopted = true,
        .allow_any_host = s.allow_any_host and s.allow_hosts.len == 0,
        .created_s = now,
        .adopted_s = now,
        .adopt_class = if (std.mem.startsWith(u8, s.subnqn, configfs.OWNERSHIP_NQN_PREFIX)) .ledger_loss else .foreign,
        .ns_uuid = s.device_uuid orelse @splat(0),
        .subnqn = subnqn,
        .backing_path = backing,
        .backing_canonical = canon,
        .listeners = listeners,
        .hosts = hosts,
    };
}

/// `adopt_verify_unchanged` (M:1097-1164): path, canonical, uuid, nsids,
/// the listener set incl. ids, the allow-hosts set.
fn liveUnchanged(a: *const configfs.LiveShare, b: *const configfs.LiveShare) bool {
    if (!std.mem.eql(u8, a.device_path, b.device_path)) return false;
    if ((a.device_uuid == null) != (b.device_uuid == null)) return false;
    if (a.device_uuid) |u| if (!std.mem.eql(u8, &u, &b.device_uuid.?)) return false;
    if (!std.mem.eql(u32, a.nsids, b.nsids)) return false;
    if (a.listeners.len != b.listeners.len or a.allow_hosts.len != b.allow_hosts.len) return false;
    for (a.listeners, b.listeners) |x, y| if (x.port_id != y.port_id or !std.mem.eql(u8, x.traddr, y.traddr) or !std.mem.eql(u8, x.trsvcid, y.trsvcid)) return false;
    for (a.allow_hosts, b.allow_hosts) |x, y| if (!std.mem.eql(u8, x, y)) return false;
    return true;
}

pub fn adopt(ctx: *Ctx, subnqn: []const u8) Outcome {
    const gpa = ctx.gpa;
    if (preflightFor(ctx, "adopt")) |r| return r;
    var walk = configfs.walkLive(ctx.fs, gpa) catch |e| return fail(ctx, "adopt: cannot walk {s}: {s}", .{ configfs.NVMET_ROOT, @errorName(e) });
    defer walk.deinit(gpa);
    var l = ctx.store.load() catch |e| return fail(ctx, "adopt: cannot load the ledger: {s}", .{@errorName(e)});
    defer l.deinit(gpa);
    const s = walk.find(subnqn) orelse return adoptFail(ctx, .adopt_not_live, "{s} is not live on this host's target (a ledgered-but-down share is `restore` territory)", .{subnqn});
    if (configfs.isHarnessNqn(subnqn)) return adoptFail(ctx, .adopt_harness_owned, "{s} carries a harness marker — harness objects belong to their harness's teardown", .{subnqn});
    for (s.listeners) |li| if (configfs.isHarnessPortId(li.port_id)) return adoptFail(ctx, .adopt_harness_owned, "{s} rides harness port id {d} (the fidelity slice {d}–{d}) — harness objects belong to their harness's teardown", .{ subnqn, li.port_id, configfs.HARNESS_PORT_ID_BASE, configfs.HARNESS_PORT_ID_BASE + configfs.HARNESS_PORT_ID_SLICE - 1 });
    if (l.find(subnqn)) |r| return adoptFail(ctx, .adopt_already_ledgered, "{s} is already recorded ({s}) — {s}", .{ subnqn, @tagName(r.state), if (r.state == .active) "`unshare` or `restore`" else "`restore`" });
    if (!s.materialized()) return adoptFail(ctx, .adopt_shape_unsupported, "{s} has no materialized namespace (no device_path, not enabled) — remove it and re-share", .{subnqn});
    if (s.nsids.len != 1 or s.nsids[0] != configfs.NAMESPACE_INDEX) return adoptFail(ctx, .adopt_shape_unsupported, "{s} has {d} namespace(s); the shape law is exactly namespaces/1 — remove it and re-share", .{ subnqn, s.nsids.len });
    if (s.listeners.len == 0) return adoptFail(ctx, .adopt_shape_unsupported, "{s} has no listener (no port links a subsystem with no fabric presence to anything an initiator can reach) — remove it and re-share", .{subnqn});
    const canonical = (uring_fs.canonicalize(gpa, s.device_path) catch return fail(ctx, "out of memory", .{})) orelse (gpa.dupe(u8, s.device_path) catch return fail(ctx, "out of memory", .{}));
    defer gpa.free(canonical);
    if (l.findByBacking(canonical)) |r| return adoptFail(ctx, .adopt_already_ledgered, "the backing {s} is already recorded under {s} ({s})", .{ canonical, r.subnqn, @tagName(r.state) });
    for (walk.shares) |*o| {
        if (o == s or !o.materialized()) continue;
        if (std.mem.eql(u8, o.device_path, s.device_path) or std.mem.eql(u8, o.device_path, canonical)) return adoptFail(ctx, .adopt_backing_duplicated, "{s} and {s} both serve {s} — absorbing one of two same-backing servers would bless the double-serve; remove one first", .{ subnqn, o.subnqn, s.device_path });
    }
    var cand = buildCandidate(ctx, s, canonical) catch |e| switch (e) {
        error.ShapeUnsupported => return adoptFail(ctx, .adopt_shape_unsupported, "{s}: a listener's transport/address is not a shape the ledger represents — remove it and re-share", .{subnqn}),
        else => return fail(ctx, "adopt: out of memory", .{}),
    };
    defer cand.deinit(gpa);
    if (s.device_uuid == null) log.warn("nvmeof adopt: {s} serves WITHOUT a device_uuid — recorded as nil; restart-identity stability needs a re-share", .{subnqn});
    for (cand.listeners) |li| if (li.port_id < ctx.port_base or li.port_id >= ctx.port_base + configfs.PORT_ID_RANGE) log.info("nvmeof adopt: {s} rides port id {d}, outside the reserved range [{d}, {d}] — recorded as-is (the range governs only allocation; the link-free teardown law covers removal)", .{ subnqn, li.port_id, ctx.port_base, ctx.port_base + configfs.PORT_ID_RANGE - 1 });
    const Begin = struct { gpa: Allocator, rec: *const Record };
    ctx.store.mutate(Begin{ .gpa = gpa, .rec = &cand }, struct {
        fn f(c: Begin, lg: *ledger.Ledger) anyerror!void {
            try ledger.beginShare(c.gpa, lg, c.rec);
        }
    }.f) catch |e| return fail(ctx, "adopt: the pending record could not be written: {s} (no target state was touched)", .{@errorName(e)});
    // TOCTOU: the walk AGAIN; any drift aborts and GCs the pending record.
    var again = configfs.walkLive(ctx.fs, gpa) catch |e| {
        deleteRecord(ctx, subnqn) catch {};
        return fail(ctx, "adopt: the re-probe failed ({s}) — the pending record was removed; no target state was touched", .{@errorName(e)});
    };
    defer again.deinit(gpa);
    const s2 = again.find(subnqn);
    if (s2 == null or !liveUnchanged(s, s2.?)) {
        deleteRecord(ctx, subnqn) catch {};
        return adoptFail(ctx, .adopt_toctou_drift, "{s} changed between the probe and the record (TOCTOU drift) — the pending record was removed; no target state was touched; retry", .{subnqn});
    }
    finalizeRecord(ctx, subnqn) catch |e| return fail(ctx, "adopt: {s} recorded pending but could not flip active ({s}) — `squeezefs nvmeof restore` finalizes it", .{ subnqn, @errorName(e) });
    stats.nvmet_adopts.inc();
    ctx.stdout.print("adopted {s} ({s}; {d} listener(s); {s})\n", .{ subnqn, @tagName(cand.adopt_class), cand.listeners.len, if (cand.allow_any_host) "allow-any" else "allow-list" }) catch {};
    ctx.stdout.flush() catch {};
    return .ok;
}

// ---------------------------------------------------------------------------
// target {status, setup, systemd-unit, stop}
// ---------------------------------------------------------------------------

pub fn targetStatus(ctx: *Ctx, json: bool) Outcome {
    const st = configfs.status(ctx.fs, ctx.gpa) catch |e| return fail(ctx, "target status: {s}", .{@errorName(e)});
    if (json) {
        st.writeJson(ctx.stdout) catch {};
        ctx.stdout.writeAll("\n") catch {};
    } else {
        ctx.stdout.print("configfs mounted: {}\nnvmet present: {}\nsubsystems: {d}\nnamespaces: {d} ({d} enabled, {d} with resv_enable)\nports: {d}\nhosts: {d}\n", .{ st.configfs_mounted, st.nvmet_present, st.subsystems, st.namespaces, st.enabled_namespaces, st.resv_enabled_namespaces, st.ports, st.hosts }) catch {};
    }
    ctx.stdout.flush() catch {};
    return .ok;
}

/// `target setup`: the preflight alone (configfs mounted, the nvmet dir
/// present) — the product loads no module.
pub fn targetSetup(ctx: *Ctx) Outcome {
    if (preflightFor(ctx, "target setup")) |r| return r;
    ctx.stdout.print("kernel nvmet target ready at {s}\n", .{configfs.NVMET_ROOT}) catch {};
    ctx.stdout.flush() catch {};
    return .ok;
}

pub fn targetStop(ctx: *Ctx) Outcome {
    return fail(ctx, "target stop: the kernel nvmet target is not a process — `squeezefs nvmeof unshare <subnqn>` releases a share; the kernel keeps serving the rest", .{});
}

/// The oneshot unit to stdout, values baked, never installed.
pub fn targetSystemdUnit(ctx: *Ctx, exe: []const u8) Outcome {
    ctx.stdout.print(
        \\[Unit]
        \\Description=SqueezeFS NVMe-oF target restore (kernel nvmet via configfs)
        \\Wants=network-online.target
        \\After=network-online.target
        \\
        \\[Service]
        \\Type=oneshot
        \\RemainAfterExit=yes
        \\ExecStart={s} nvmeof restore
        \\
        \\[Install]
        \\WantedBy=multi-user.target
        \\
    , .{exe}) catch {};
    ctx.stdout.flush() catch {};
    return .ok;
}

// ---------------------------------------------------------------------------
// Tests over the fake configfs tree and a scratch ledger.
// ---------------------------------------------------------------------------

const testing = std.testing;

const Rig = struct {
    gpa: Allocator,
    fake: configfs.FakeFs,
    pool: *uring_fs.Pool,
    dir: [:0]u8,
    store: ledger.Store,
    out: Io.Writer.Allocating,
    err: Io.Writer.Allocating,
    ctx: Ctx,
    backing: [:0]u8,

    fn init(gpa: Allocator) !*Rig {
        const self = try gpa.create(Rig);
        errdefer gpa.destroy(self);
        _ = linux.mkdir("/build/tmp", 0o755);
        self.gpa = gpa;
        self.fake = try configfs.FakeFs.init(gpa);
        self.dir = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/nvmet-verbs-{d}", .{linux.getpid()}, 0);
        Io.Dir.cwd().deleteTree(testing.io, self.dir) catch {};
        self.pool = try uring_fs.Pool.init(gpa, uring_fs.Config.fixed(1, uring_fs.nofileSoftAsFound()));
        self.store = try ledger.Store.init(gpa, self.pool, self.dir);
        self.out = .init(gpa);
        self.err = .init(gpa);
        self.backing = try std.fmt.allocPrintSentinel(gpa, "/build/tmp/nvmet-verbs-{d}-a.img", .{linux.getpid()}, 0);
        _ = linux.unlink(self.backing.ptr);
        self.ctx = .{ .gpa = gpa, .io = testing.io, .fs = self.fake.face(), .store = &self.store, .port_base = configfs.PORT_ID_BASE_DEFAULT, .stdout = &self.out.writer, .stderr = &self.err.writer, .random = fixedRandom };
        return self;
    }

    fn fixedRandom(_: *Ctx, out: *[16]u8) bool {
        out.* = .{ 0xde, 0xad, 0xbe, 0xef, 0, 0, 0x40, 0, 0x80, 0, 0, 0, 0, 0, 0, 1 };
        return true;
    }

    fn deinit(self: *Rig) void {
        _ = linux.unlink(self.backing.ptr);
        Io.Dir.cwd().deleteTree(testing.io, self.dir) catch {};
        self.out.deinit();
        self.err.deinit();
        self.pool.deinit();
        self.fake.deinit();
        self.gpa.free(self.dir);
        self.gpa.free(self.backing);
        self.gpa.destroy(self);
    }

    fn reset(self: *Rig) void {
        self.out.clearRetainingCapacity();
        self.err.clearRetainingCapacity();
    }

    fn ledgerOf(self: *Rig) !ledger.Ledger {
        return self.store.load();
    }
};

fn initRig(gpa: Allocator) !*Rig {
    return Rig.init(gpa) catch |e| switch (e) {
        error.RingSetupFailed => testkit.skip(@src(), .no_kernel_feature, "the io_uring file layer's ring did not set up on this host"),
        else => e,
    };
}

const fake_ready = struct {
    /// The verbs' preflight probes the REAL tree; the fake-tree tests run
    /// where the real one is present (the venue as root) and skip loud
    /// elsewhere.
    fn check() !void {
        var e: ?linux.E = null;
        if (linux.geteuid() != 0) return testkit.skip(@src(), .no_root, "the nvmet configfs tree needs root");
        if (configfs.preflight(&e) != .ready) return testkit.skip(@src(), .no_kernel_feature, "the kernel nvmet target is not ready (configfs preflight)");
    }
};

test "share → list → unshare over the fake tree: the pending record precedes the first mutation, the active record follows the last; list classifies managed+live; unshare leaves zero residue and an empty ledger; an unledgered unshare refuses naming adopt and the manual steps; the default NQN is the canonical backing's hash" {
    try fake_ready.check();
    const gpa = testing.allocator;
    var rig = try initRig(gpa);
    defer rig.deinit();
    const nqn = "nqn.2026-10.io.squeezefs:share-test1";
    try testing.expectEqual(Outcome.ok, share(&rig.ctx, .{ .backing_path = rig.backing, .subnqn = nqn, .trtype = .loop, .create_size = 64 << 20 }));
    try testing.expect(std.mem.indexOf(u8, rig.out.written(), "shared " ++ nqn) != null);
    try testing.expect(std.mem.indexOf(u8, rig.out.written(), "ns_uuid deadbeef-0000-4000-8000-000000000001") != null);
    // The op log: mkdir after the ledger's first write — the pending record is generation 1, the active is generation 2.
    var l = try rig.ledgerOf();
    defer l.deinit(gpa);
    try testing.expectEqual(@as(u64, 2), l.generation);
    try testing.expectEqual(ledger.State.active, l.find(nqn).?.state);
    try testing.expectEqualStrings(rig.backing, l.find(nqn).?.backing_canonical);
    rig.reset();
    try testing.expectEqual(Outcome.ok, list(&rig.ctx, false));
    try testing.expect(std.mem.indexOf(u8, rig.out.written(), "live; managed") != null);
    rig.reset();
    try testing.expectEqual(Outcome.ok, list(&rig.ctx, true));
    try testing.expect(std.mem.indexOf(u8, rig.out.written(), "\"classification\":\"managed\"") != null);
    try testing.expect(std.mem.indexOf(u8, rig.out.written(), "\"live\":true") != null);
    // A second share of the same backing refuses naming the holder's exit.
    rig.reset();
    try testing.expectEqual(Outcome.failed, share(&rig.ctx, .{ .backing_path = rig.backing, .subnqn = "nqn.2026-10.io.squeezefs:share-test2", .trtype = .loop }));
    try testing.expect(std.mem.indexOf(u8, rig.err.written(), "never be double-served") != null);
    try testing.expect(std.mem.indexOf(u8, rig.err.written(), "nvmeof unshare " ++ nqn) != null);
    // The same NQN squat refuses too.
    rig.reset();
    try testing.expectEqual(Outcome.failed, share(&rig.ctx, .{ .backing_path = "/build/tmp/other.img", .subnqn = nqn, .trtype = .loop, .create_size = 1 << 20 }));
    try testing.expect(std.mem.indexOf(u8, rig.err.written(), "already live") != null);
    _ = linux.unlink("/build/tmp/other.img");
    // Unshare: zero residue, ledger empty.
    rig.reset();
    try testing.expectEqual(Outcome.ok, unshare(&rig.ctx, nqn));
    try testing.expectEqual(@as(usize, 0), rig.fake.objectCount());
    var l2 = try rig.ledgerOf();
    defer l2.deinit(gpa);
    try testing.expectEqual(@as(usize, 0), l2.records.items.len);
    rig.reset();
    try testing.expectEqual(Outcome.failed, unshare(&rig.ctx, nqn));
    try testing.expect(std.mem.indexOf(u8, rig.err.written(), "never tears down target objects it did not record") != null);
    try testing.expect(std.mem.indexOf(u8, rig.err.written(), "nvmeof adopt") != null);
    var nb: [128]u8 = undefined;
    const d = defaultSubnqn(&nb, "/build/x");
    try testing.expect(std.mem.startsWith(u8, d, configfs.OWNERSHIP_NQN_PREFIX));
    try testing.expectEqual(configfs.OWNERSHIP_NQN_PREFIX.len + 16, d.len);
}

test "loudfail: a missing backing without --create-size; a '..' allow-host; a malformed --ip; loop with --ip; tcp without --ip; a bad --subnqn — each refusal names its remediation and mutates nothing" {
    try fake_ready.check();
    const gpa = testing.allocator;
    var rig = try initRig(gpa);
    defer rig.deinit();
    try testing.expectEqual(Outcome.failed, share(&rig.ctx, .{ .backing_path = "/build/tmp/does-not-exist.img", .trtype = .loop }));
    try testing.expect(std.mem.indexOf(u8, rig.err.written(), "--create-size") != null);
    rig.reset();
    try testing.expectEqual(Outcome.failed, share(&rig.ctx, .{ .backing_path = rig.backing, .trtype = .loop, .allow_hosts = &.{"../x"}, .create_size = 1 << 20 }));
    try testing.expect(std.mem.indexOf(u8, rig.err.written(), "--allow-host") != null);
    rig.reset();
    try testing.expectEqual(Outcome.failed, share(&rig.ctx, .{ .backing_path = rig.backing, .ips = &.{"not-an-ip"}, .create_size = 1 << 20 }));
    try testing.expect(std.mem.indexOf(u8, rig.err.written(), "not an IPv4/IPv6 address") != null);
    rig.reset();
    try testing.expectEqual(Outcome.failed, share(&rig.ctx, .{ .backing_path = rig.backing, .trtype = .loop, .ips = &.{"127.0.0.1"}, .create_size = 1 << 20 }));
    try testing.expect(std.mem.indexOf(u8, rig.err.written(), "takes no --ip") != null);
    rig.reset();
    try testing.expectEqual(Outcome.failed, share(&rig.ctx, .{ .backing_path = rig.backing, .create_size = 1 << 20 }));
    try testing.expect(std.mem.indexOf(u8, rig.err.written(), "at least one --ip") != null);
    rig.reset();
    try testing.expectEqual(Outcome.failed, share(&rig.ctx, .{ .backing_path = rig.backing, .subnqn = "bad nqn", .trtype = .loop, .create_size = 1 << 20 }));
    try testing.expect(std.mem.indexOf(u8, rig.err.written(), "--subnqn") != null);
    try testing.expectEqual(@as(usize, 0), rig.fake.objectCount());
    var l = try rig.ledgerOf();
    defer l.deinit(gpa);
    try testing.expectEqual(@as(usize, 0), l.records.items.len);
    rig.reset();
    try testing.expectEqual(Outcome.failed, targetStop(&rig.ctx));
    try testing.expect(std.mem.indexOf(u8, rig.err.written(), "not a process") != null);
    rig.reset();
    try testing.expectEqual(Outcome.ok, targetSystemdUnit(&rig.ctx, "/usr/local/bin/squeezefs"));
    try testing.expect(std.mem.indexOf(u8, rig.out.written(), "ExecStart=/usr/local/bin/squeezefs nvmeof restore") != null);
    try testing.expect(std.mem.indexOf(u8, rig.out.written(), "Type=oneshot") != null);
}

test "restore reconciles every intent state: removing resumed; pending finalized when live matches / garbage-collected when absent / FAILED on a mismatch (never clobbered); active no-op when live / re-established when gone / FAILED on a mismatch; a second restore is all no-ops" {
    try fake_ready.check();
    const gpa = testing.allocator;
    var rig = try initRig(gpa);
    defer rig.deinit();
    const nqn = "nqn.2026-10.io.squeezefs:share-r1";
    try testing.expectEqual(Outcome.ok, share(&rig.ctx, .{ .backing_path = rig.backing, .subnqn = nqn, .trtype = .loop, .create_size = 1 << 20 }));
    // Active + live: verified no-op.
    rig.reset();
    try testing.expectEqual(Outcome.ok, restore(&rig.ctx));
    try testing.expect(std.mem.indexOf(u8, rig.out.written(), "verified no-op") != null);
    // Active + gone (configfs wiped by hand): restored with the recorded uuid and id.
    rig.fake.deinit();
    rig.fake = try configfs.FakeFs.init(gpa);
    rig.ctx.fs = rig.fake.face();
    rig.reset();
    try testing.expectEqual(Outcome.ok, restore(&rig.ctx));
    try testing.expect(std.mem.indexOf(u8, rig.out.written(), ": restored") != null);
    var b: [64]u8 = undefined;
    try testing.expectEqualStrings("deadbeef-0000-4000-8000-000000000001", try rig.ctx.fs.readAttr("subsystems/" ++ nqn ++ "/namespaces/1/device_uuid", &b));
    // Active + live but MISMATCHED (a foreign rebuild under the same NQN with another uuid): FAILED, nothing clobbered.
    rig.fake.deinit();
    rig.fake = try configfs.FakeFs.init(gpa);
    rig.ctx.fs = rig.fake.face();
    var foreign = Listener.loop;
    foreign.port_id = 1;
    _ = try configfs.apply(rig.ctx.fs, .{ .subnqn = nqn, .backing_path = rig.backing, .ns_uuid = @splat(9), .allow_hosts = &.{}, .listeners = &.{foreign} });
    rig.reset();
    try testing.expectEqual(Outcome.failed, restore(&rig.ctx));
    try testing.expect(std.mem.indexOf(u8, rig.out.written(), "MISMATCHES its record") != null);
    try testing.expectEqualStrings("09090909-0909-0909-0909-090909090909", try rig.ctx.fs.readAttr("subsystems/" ++ nqn ++ "/namespaces/1/device_uuid", &b));
    // Pending + mismatched live: FAILED too; pending + absent: garbage-collected; pending + matching: finalized.
    try rig.store.mutate({}, struct {
        fn f(_: void, lg: *ledger.Ledger) anyerror!void {
            lg.find("nqn.2026-10.io.squeezefs:share-r1").?.state = .pending;
        }
    }.f);
    rig.reset();
    try testing.expectEqual(Outcome.failed, restore(&rig.ctx));
    try testing.expect(std.mem.indexOf(u8, rig.out.written(), "DO NOT match") != null);
    rig.fake.deinit();
    rig.fake = try configfs.FakeFs.init(gpa);
    rig.ctx.fs = rig.fake.face();
    rig.reset();
    try testing.expectEqual(Outcome.ok, restore(&rig.ctx));
    try testing.expect(std.mem.indexOf(u8, rig.out.written(), "garbage-collected") != null);
    var l = try rig.ledgerOf();
    defer l.deinit(gpa);
    try testing.expectEqual(@as(usize, 0), l.records.items.len);
    // Pending + matching live (a crash between the last link and the finalize): finalized.
    rig.reset();
    try testing.expectEqual(Outcome.ok, share(&rig.ctx, .{ .backing_path = rig.backing, .subnqn = nqn, .trtype = .loop }));
    try rig.store.mutate({}, struct {
        fn f(_: void, lg: *ledger.Ledger) anyerror!void {
            lg.find("nqn.2026-10.io.squeezefs:share-r1").?.state = .pending;
        }
    }.f);
    rig.reset();
    try testing.expectEqual(Outcome.ok, restore(&rig.ctx));
    try testing.expect(std.mem.indexOf(u8, rig.out.written(), "pending intent finalized") != null);
    // Removing: the teardown resumes and the record goes.
    try rig.store.mutate({}, struct {
        fn f(_: void, lg: *ledger.Ledger) anyerror!void {
            lg.find("nqn.2026-10.io.squeezefs:share-r1").?.state = .removing;
        }
    }.f);
    rig.reset();
    try testing.expectEqual(Outcome.ok, restore(&rig.ctx));
    try testing.expect(std.mem.indexOf(u8, rig.out.written(), "teardown resumed") != null);
    try testing.expectEqual(@as(usize, 0), rig.fake.objectCount());
    rig.reset();
    try testing.expectEqual(Outcome.ok, restore(&rig.ctx));
    try testing.expect(std.mem.indexOf(u8, rig.out.written(), "nothing to restore") != null);
}

test "adopt: a hand-built foreign share with a small-int port id is recorded (live uuid, out-of-range id, ADOPTED/foreign) and configfs is untouched; re-adopt refuses [adopt_already_ledgered]; a harness NQN refuses [adopt_harness_owned]; not-live, no-listener and two-namespace shapes refuse by token; the ledger-loss class names the product prefix; unshare then removes the small-int port" {
    try fake_ready.check();
    const gpa = testing.allocator;
    var rig = try initRig(gpa);
    defer rig.deinit();
    const fs = rig.ctx.fs;
    const nqn = "nqn.2026-10.example:hand-built";
    var hand = Listener.loop;
    hand.port_id = 3;
    try Io.Dir.cwd().writeFile(testing.io, .{ .sub_path = rig.backing, .data = "x" });
    _ = try configfs.apply(fs, .{ .subnqn = nqn, .backing_path = rig.backing, .ns_uuid = @splat(7), .allow_hosts = &.{}, .listeners = &.{hand} });
    const before = try rig.fake.opLog(gpa);
    defer gpa.free(before);
    rig.fake.clearOps();
    try testing.expectEqual(Outcome.ok, adopt(&rig.ctx, nqn));
    try testing.expect(std.mem.indexOf(u8, rig.out.written(), "adopted " ++ nqn ++ " (foreign") != null);
    // Zero configfs mutations during adopt (reads are not recorded).
    try testing.expectEqual(@as(usize, 0), rig.fake.ops.items.len);
    var l = try rig.ledgerOf();
    defer l.deinit(gpa);
    const r = l.find(nqn).?;
    try testing.expect(r.adopted and r.adopt_class == .foreign and r.state == .active and r.allow_any_host);
    try testing.expectEqual(@as(u16, 3), r.listeners[0].port_id);
    try testing.expectEqual(@as(u8, 7), r.ns_uuid[0]);
    rig.reset();
    try testing.expectEqual(Outcome.failed, adopt(&rig.ctx, nqn));
    try testing.expect(std.mem.indexOf(u8, rig.err.written(), "[adopt_already_ledgered]") != null);
    rig.reset();
    try testing.expectEqual(Outcome.failed, adopt(&rig.ctx, "nqn.2026-10.io.squeezefs:fideli-x"));
    try testing.expect(std.mem.indexOf(u8, rig.err.written(), "[adopt_not_live]") != null);
    var hl = Listener.loop;
    hl.port_id = 4;
    _ = try configfs.apply(fs, .{ .subnqn = "nqn.2026-10.io.squeezefs:fideli-x", .backing_path = "/build/tmp/fideli.img", .ns_uuid = @splat(8), .allow_hosts = &.{}, .listeners = &.{hl} });
    rig.reset();
    try testing.expectEqual(Outcome.failed, adopt(&rig.ctx, "nqn.2026-10.io.squeezefs:fideli-x"));
    try testing.expect(std.mem.indexOf(u8, rig.err.written(), "[adopt_harness_owned]") != null);
    // No listener.
    try fs.mkdir("subsystems/nqn.2026-10.example:nolisten");
    try fs.mkdir("subsystems/nqn.2026-10.example:nolisten/namespaces/1");
    try fs.writeAttr("subsystems/nqn.2026-10.example:nolisten/namespaces/1/device_path", "/build/tmp/nl.img");
    try fs.writeAttr("subsystems/nqn.2026-10.example:nolisten/namespaces/1/enable", "1");
    rig.reset();
    try testing.expectEqual(Outcome.failed, adopt(&rig.ctx, "nqn.2026-10.example:nolisten"));
    try testing.expect(std.mem.indexOf(u8, rig.err.written(), "[adopt_shape_unsupported]") != null and std.mem.indexOf(u8, rig.err.written(), "no listener") != null);
    // Two namespaces.
    try fs.mkdir("subsystems/nqn.2026-10.example:nolisten/namespaces/2");
    rig.reset();
    try testing.expectEqual(Outcome.failed, adopt(&rig.ctx, "nqn.2026-10.example:nolisten"));
    try testing.expect(std.mem.indexOf(u8, rig.err.written(), "2 namespace(s)") != null);
    // The ledger-loss class: a product-prefixed NQN after the ledger file vanished.
    const own = "nqn.2026-10.io.squeezefs:share-lost";
    var ol = Listener.loop;
    ol.port_id = 53500;
    _ = try configfs.apply(fs, .{ .subnqn = own, .backing_path = "/build/tmp/lost.img", .ns_uuid = @splat(5), .allow_hosts = &.{}, .listeners = &.{ol} });
    rig.reset();
    try testing.expectEqual(Outcome.ok, adopt(&rig.ctx, own));
    try testing.expect(std.mem.indexOf(u8, rig.out.written(), "(ledger_loss") != null);
    // Unshare of the adopted small-int port share removes port 3.
    rig.reset();
    try testing.expectEqual(Outcome.ok, unshare(&rig.ctx, nqn));
    try testing.expect(!fs.exists("ports/3"));
    try testing.expect(fs.exists("ports/53500"));
}

test "the crash window (law 6): a share whose second listener's link refuses leaves a PENDING record claiming BOTH port ids; a second share of the backing refuses mid-window naming the pending holder; restore finalizes the live half (the never-bound listener is not re-applied); unshare removes both recorded ports" {
    try fake_ready.check();
    const gpa = testing.allocator;
    var rig = try initRig(gpa);
    defer rig.deinit();
    const nqn = "nqn.2026-10.io.squeezefs:share-cw";
    const l2 = try Listener.tcp("192.0.2.55", 4420);
    var kb: [96]u8 = undefined;
    rig.fake.refuse_link_port = configfs.firstCandidate(configfs.PORT_ID_BASE_DEFAULT, l2.hashKey(&kb));
    try testing.expectEqual(Outcome.failed, share(&rig.ctx, .{ .backing_path = rig.backing, .subnqn = nqn, .ips = &.{ "127.0.0.1", "192.0.2.55" }, .create_size = 1 << 20 }));
    try testing.expect(std.mem.indexOf(u8, rig.err.written(), "PENDING record claims what exists") != null);
    var l = try rig.ledgerOf();
    defer l.deinit(gpa);
    const r = l.find(nqn).?;
    try testing.expectEqual(ledger.State.pending, r.state);
    try testing.expectEqual(@as(usize, 2), r.listeners.len);
    try testing.expect(r.listeners[0].port_id != r.listeners[1].port_id);
    rig.reset();
    try testing.expectEqual(Outcome.failed, share(&rig.ctx, .{ .backing_path = rig.backing, .subnqn = "nqn.2026-10.io.squeezefs:share-cw2", .trtype = .loop }));
    try testing.expect(std.mem.indexOf(u8, rig.err.written(), "PENDING share") != null);
    rig.fake.refuse_link_port = null;
    rig.reset();
    try testing.expectEqual(Outcome.ok, restore(&rig.ctx));
    try testing.expect(std.mem.indexOf(u8, rig.out.written(), "pending intent finalized") != null);
    var walk = try configfs.walkLive(rig.ctx.fs, gpa);
    defer walk.deinit(gpa);
    try testing.expectEqual(@as(usize, 1), walk.find(nqn).?.listeners.len);
    rig.reset();
    try testing.expectEqual(Outcome.ok, unshare(&rig.ctx, nqn));
    try testing.expect(std.mem.indexOf(u8, rig.out.written(), "ports removed 2") != null);
    try testing.expectEqual(@as(usize, 0), rig.fake.objectCount());
}
