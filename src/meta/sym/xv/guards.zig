//! TRAVELLING 4a GUARDS (charter §2 lock order, §5.5 "Cross-owner
//! namespace ops"; `docs/porting/sym_tokens_intents_striping.md` §2.4):
//! two laws — (1) ONE table, one canonical `lockMany`: every key whose
//! lock lives in THIS process's table rides the one stripe-canonical
//! acquisition; a key in another holder's table is one `XvGuards` to that
//! holder, served by the same `lockMany` in ITS table and PARKED under the
//! op's scope until `XvRelease`; (2) tables in ASCENDING appender-id
//! order — "a waiter for table T holds only tables < T and the wait-for
//! graph over tables is acyclic". A served STEP under a scope that COVERS
//! its stripes applies with the parked guards (`Covered`); else it takes
//! the step's keys (`Take`) — coverage by `(class, stripe)` SET, never by
//! key (`dlm.Manager.covers`), so a scope that parked unrelated keys never
//! lets a step apply unguarded.
//!
//! The holder's lease-expiry sweep is the backstop: a scope is kept while
//! its client is a LIVE member, released the sweep after the owner
//! evicts it; a client the owner does not know keeps the grace window
//! `T_owner` as the belt. PORTABLE.

const std = @import("std");
const Allocator = std.mem.Allocator;
const Io = std.Io;
const counters = @import("../../../core/counters.zig");
const errors = @import("../../../core/errors.zig");
const clock = @import("../../../core/clock.zig");
const sync = @import("../../../sync/sync.zig");
const verbs = @import("../../../wire/verbs.zig");
const service = @import("../../../wire/service.zig");
const codec = @import("../../../wire/codec.zig");
const dlm = @import("../../dlm.zig");
const token_cache = @import("../tokens/token_cache.zig");
const token_table = @import("../tokens/token_table.zig");

const assert = std.debug.assert;

pub const Link = token_cache.Link;
pub const Scope = u64;
pub const AppenderId = u32;

pub const stats = struct {
    /// `XvGuards` asks sent (one per foreign holder table per op).
    pub var xv_cross_owner_guard_rpcs: counters.ShardedCounter = .{};
    /// A holder answered `StaleHolderView`: every key's slot re-resolved.
    pub var xv_cross_owner_guard_stale_reresolves: counters.ShardedCounter = .{};
    /// Scopes parked at this holder (served `XvGuards`).
    pub var xv_cross_owner_guards_parked: counters.ShardedCounter = .{};
    /// A parked scope released by the expiry sweep, never by its client:
    /// must stay 0.
    pub var xv_cross_owner_guard_expiries: counters.Tripwire = .init("xv_cross_owner_guard_expiries");
    /// Served steps that applied under the scope's parked guards.
    pub var xv_cross_owner_steps_covered: counters.ShardedCounter = .{};
    /// Served steps that took their own guards (the scope did not cover).
    pub var xv_cross_owner_steps_took_guards: counters.ShardedCounter = .{};
    /// The round trip of a guard ask.
    pub var xv_cross_owner_guard_rtt_ns: counters.ShardedLatencyHistogram = .{};
};

/// `mint_guard_scope = seq & 2^40−1 | 2^40`: nonzero by construction (0 on
/// the wire = "no scope travelled").
pub fn mintScope(seq: u64) Scope {
    return (seq & ((1 << 40) - 1)) | (1 << 40);
}

/// `stuck_grace = T_owner`: a client the owner does not know keeps the
/// grace window as the belt.
pub const SCOPE_GRACE_NS: u64 = token_table.T_OWNER_NS;

/// A holder re-resolves on a stale view at most this many times: one
/// legal move + one re-move; a third stale answer is the retryable class.
pub const STALE_HOLDER_RETRIES: u32 = 2;

pub const Error = error{ OutOfMemory, PlanTooLarge, GuardRefused, HolderUnreachable, StaleHolderView };

// ---------------------------------------------------------------------------
// The holder's parked scopes.
// ---------------------------------------------------------------------------

const Parked = struct {
    client: u64,
    set: dlm.WideGuardSet,
    since_ns: u64,
};

pub const ServeGuards = enum { covered, take };

/// Who the holder asks whether a client is live (the membership owner's
/// verdict; null = in-process contracts: every client is live).
pub const Liveness = struct {
    ctx: *anyopaque,
    isLive: *const fn (ctx: *anyopaque, client: u64) bool,
};

pub const ParkedScopes = struct {
    gpa: Allocator,
    io: Io,
    locks: *dlm.Manager,
    lock: sync.Mutex = .init,
    map: std.AutoHashMapUnmanaged(Scope, Parked) = .empty,
    liveness: ?Liveness = null,

    pub fn init(gpa: Allocator, io: Io, locks: *dlm.Manager) ParkedScopes {
        return .{ .gpa = gpa, .io = io, .locks = locks };
    }

    pub fn deinit(self: *ParkedScopes) void {
        var it = self.map.valueIterator();
        while (it.next()) |p| p.set.release(self.io);
        self.map.deinit(self.gpa);
    }

    /// Serve `XvGuards`: `lockMany` in THIS table of the requests the
    /// scope does not already cover (a second call under one scope must
    /// never wait behind itself), parked under `scope`.
    pub fn serve(self: *ParkedScopes, scope: Scope, client: u64, inodes: []const dlm.InodeReq, dentries: []const dlm.DentryReq) Error!void {
        assert(scope != 0);
        // Which requests are already covered? Judged under the map lock,
        // taken OUTSIDE it (the acquisition parks).
        var need_i: [dlm.wide_requests]dlm.InodeReq = undefined;
        var ni: usize = 0;
        var need_d: [dlm.wide_requests]dlm.DentryReq = undefined;
        var nd: usize = 0;
        {
            self.lock.lockUncancelable(self.io);
            defer self.lock.unlock(self.io);
            const existing = self.map.getPtr(scope);
            for (inodes) |r| {
                if (existing) |p| if (self.locks.covers(dlm.wide_requests, &p.set, &.{r}, &.{})) continue;
                if (ni == need_i.len) return error.PlanTooLarge;
                need_i[ni] = r;
                ni += 1;
            }
            for (dentries) |r| {
                if (existing) |p| if (self.locks.covers(dlm.wide_requests, &p.set, &.{}, &.{r})) continue;
                if (nd == need_d.len) return error.PlanTooLarge;
                need_d[nd] = r;
                nd += 1;
            }
        }
        if (ni == 0 and nd == 0) return;
        var set = self.locks.lockManyWide(self.io, need_i[0..ni], need_d[0..nd]) catch return error.PlanTooLarge;
        errdefer set.release(self.io);
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        const gop = self.map.getOrPut(self.gpa, scope) catch return error.OutOfMemory;
        if (!gop.found_existing) {
            gop.value_ptr.* = .{ .client = client, .set = set, .since_ns = clock.monotonicNs() };
            stats.xv_cross_owner_guards_parked.inc();
            return;
        }
        // Merge: the parked set grows by the stripes just taken (bounded
        // by the wide capacity — the plan bound shared with the intent).
        const p = gop.value_ptr;
        for (set.slice()) |g| {
            if (p.set.len == p.set.guards.len) return error.PlanTooLarge;
            p.set.guards[p.set.len] = g;
            p.set.len += 1;
        }
    }

    /// `XvRelease`: the scope's guards drop; false when nothing was parked.
    pub fn release(self: *ParkedScopes, scope: Scope) bool {
        self.lock.lockUncancelable(self.io);
        const entry = self.map.fetchRemove(scope);
        self.lock.unlock(self.io);
        var e = entry orelse return false;
        e.value.set.release(self.io);
        return true;
    }

    /// The served step's verdict: the scope covers every request ⇒ apply
    /// with the parked guards; else take the step's own.
    pub fn verdict(self: *ParkedScopes, scope: Scope, inodes: []const dlm.InodeReq, dentries: []const dlm.DentryReq) ServeGuards {
        if (scope == 0) return .take;
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        const p = self.map.getPtr(scope) orelse return .take;
        return if (self.locks.covers(dlm.wide_requests, &p.set, inodes, dentries)) .covered else .take;
    }

    /// The expiry sweep: a scope whose client the owner lists dead (or
    /// unknown past the grace window) is released — the backstop, never
    /// the path.
    pub fn sweepExpired(self: *ParkedScopes, now_ns: u64) u32 {
        var doomed: [64]Scope = undefined;
        var n: usize = 0;
        {
            self.lock.lockUncancelable(self.io);
            defer self.lock.unlock(self.io);
            var it = self.map.iterator();
            while (it.next()) |e| {
                if (n == doomed.len) break;
                const live = if (self.liveness) |l| l.isLive(l.ctx, e.value_ptr.client) else true;
                const aged = now_ns -| e.value_ptr.since_ns > SCOPE_GRACE_NS;
                if (!live or aged) {
                    doomed[n] = e.key_ptr.*;
                    n += 1;
                }
            }
        }
        for (doomed[0..n]) |s| if (self.release(s)) stats.xv_cross_owner_guard_expiries.fire("a parked guard scope outlived its client");
        return @intCast(n);
    }

    pub fn count(self: *ParkedScopes) usize {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        return self.map.count();
    }

    pub fn isParked(self: *ParkedScopes, scope: Scope) bool {
        self.lock.lockUncancelable(self.io);
        defer self.lock.unlock(self.io);
        return self.map.contains(scope);
    }
};

/// The holder's wire face for the two guard verbs.
pub const GuardsService = struct {
    parked: *ParkedScopes,
    volume: u16,

    pub fn register(self: *GuardsService, svc: *service.Service) void {
        svc.register(.xv_guards, .{ .ctx = self, .serve = &serveGuards });
        svc.register(.xv_release, .{ .ctx = self, .serve = &serveRelease });
    }

    fn serveGuards(ctx: *anyopaque, req: *const service.Request, _: []u8) service.Answer {
        const self: *GuardsService = @ptrCast(@alignCast(ctx));
        const r = req.payload.xv_guards;
        if (r.head.volume != self.volume) return service.Answer.rejected("xv_guards names a volume this node does not hold");
        var inodes: [dlm.wide_requests]dlm.InodeReq = undefined;
        for (r.inodes, 0..) |w, i| inodes[i] = .{ .ino = w.ino, .mode = if (w.exclusive != 0) .exclusive else .shared };
        var dents: [dlm.wide_requests]dlm.DentryReq = undefined;
        var nd: usize = 0;
        var it = r.dentries();
        while (it.next()) |d| : (nd += 1) dents[nd] = .{ .parent = d.head.parent, .name = d.name, .mode = if (d.head.exclusive != 0) .exclusive else .shared };
        const client: u64 = if (req.peer) |p| p.writer_id else 0;
        self.parked.serve(r.head.scope, client, inodes[0..r.inodes.len], dents[0..nd]) catch |e| return switch (e) {
            error.PlanTooLarge => service.Answer.rejected("xv_guards plan past the wide bound"),
            error.OutOfMemory => service.Answer.refused(error.OutOfMemory, "out of memory parking the scope"),
            else => unreachable,
        };
        return .{ .ok = .{ .xv_guards = .fresh } };
    }

    fn serveRelease(ctx: *anyopaque, req: *const service.Request, _: []u8) service.Answer {
        const self: *GuardsService = @ptrCast(@alignCast(ctx));
        const r = req.payload.xv_release;
        const was = self.parked.release(r.scope);
        return .{ .ok = .{ .xv_release = if (was) .fresh else .replay } };
    }
};

// ---------------------------------------------------------------------------
// The initiator's acquisition.
// ---------------------------------------------------------------------------

/// Whose table a key's lock lives in.
pub const Table = union(enum) {
    local,
    remote: struct { holder: AppenderId, link: Link },
    /// The holder has no bound endpoint: the ask is refused retryable.
    unreachable_: AppenderId,
};

/// The resolver of a GLOBAL ino's table (the slot's holder).
pub const Tables = struct {
    ctx: *anyopaque,
    tableOf: *const fn (ctx: *anyopaque, global_ino: u64) Table,
};

const Remote = struct { holder: AppenderId, link: Link };

/// A held travelling set: the local canonical set plus one parked scope
/// per remote table; released in reverse (the remote releases are
/// fire-and-forget — the holder's sweep is the belt).
pub const Travelling = struct {
    io: Io,
    scope: Scope,
    volume: u16,
    local: dlm.GuardSet = .{},
    remotes: std.ArrayListUnmanaged(Remote) = .empty,

    pub fn release(self: *Travelling, gpa: Allocator) void {
        self.local.release(self.io);
        for (self.remotes.items) |r| {
            _ = r.link.call(r.link.ctx, .{ .xv_release = .{ .volume = self.volume, .reserved = 0, .reserved2 = 0, .scope = self.scope, .ino = 0 } }) catch {};
        }
        self.remotes.deinit(gpa);
    }

    pub fn remoteCount(self: *const Travelling) usize {
        return self.remotes.items.len;
    }
};

pub const AcquireArgs = struct {
    own_id: AppenderId,
    volume: u16,
    scope: Scope,
    inodes: []const dlm.InodeReq,
    dentries: []const dlm.DentryReq,
    /// Unlink's discovery phase acquires NO foreign table (the op pays ONE
    /// round trip per holder, at its full acquisition).
    discovery: bool = false,
};

/// Acquire the op's set across tables in ascending appender-id order: the
/// local table at `own_id`'s rank, each remote set ONE `XvGuards`. A
/// refused/unreachable ask releases everything taken so far.
pub fn acquire(gpa: Allocator, io: Io, locks: *dlm.Manager, tables: Tables, a: AcquireArgs) Error!Travelling {
    assert(a.scope != 0);
    var t: Travelling = .{ .io = io, .scope = a.scope, .volume = a.volume };
    errdefer t.release(gpa);
    // The distinct tables, ascending by appender id.
    var ids: [dlm.wide_requests * 2]AppenderId = undefined;
    var links: [dlm.wide_requests * 2]?Link = undefined;
    var n: usize = 0;
    var has_local = false;
    for (a.inodes) |r| try noteTable(tables, r.ino, a.own_id, &ids, &links, &n, &has_local);
    for (a.dentries) |r| try noteTable(tables, r.parent, a.own_id, &ids, &links, &n, &has_local);
    sortTables(ids[0..n], links[0..n]);
    var local_taken = false;
    for (ids[0..n], links[0..n]) |id, link_opt| {
        if (a.discovery and id != a.own_id) continue;
        if (has_local and !local_taken and a.own_id < id) {
            t.local = try lockLocal(locks, io, tables, a);
            local_taken = true;
        }
        if (id == a.own_id) continue;
        const link = link_opt orelse return error.HolderUnreachable;
        try askRemote(gpa, io, &t, tables, a, id, link);
    }
    if (has_local and !local_taken) t.local = try lockLocal(locks, io, tables, a);
    return t;
}

fn noteTable(tables: Tables, ino: u64, own: AppenderId, ids: []AppenderId, links: []?Link, n: *usize, has_local: *bool) Error!void {
    switch (tables.tableOf(tables.ctx, ino)) {
        .local => {
            has_local.* = true;
            try pushId(ids, links, n, own, null);
        },
        .remote => |r| try pushId(ids, links, n, r.holder, r.link),
        .unreachable_ => return error.HolderUnreachable,
    }
}

fn pushId(ids: []AppenderId, links: []?Link, n: *usize, id: AppenderId, link: ?Link) Error!void {
    for (ids[0..n.*]) |x| if (x == id) return;
    if (n.* == ids.len) return error.PlanTooLarge;
    ids[n.*] = id;
    links[n.*] = link;
    n.* += 1;
}

fn sortTables(ids: []AppenderId, links: []?Link) void {
    // Insertion sort: ≤ a handful of tables per op.
    var i: usize = 1;
    while (i < ids.len) : (i += 1) {
        var j = i;
        while (j > 0 and ids[j - 1] > ids[j]) : (j -= 1) {
            std.mem.swap(AppenderId, &ids[j - 1], &ids[j]);
            std.mem.swap(?Link, &links[j - 1], &links[j]);
        }
    }
}

fn lockLocal(locks: *dlm.Manager, io: Io, tables: Tables, a: AcquireArgs) Error!dlm.GuardSet {
    var inodes: [dlm.max_requests]dlm.InodeReq = undefined;
    var ni: usize = 0;
    var dents: [dlm.max_requests]dlm.DentryReq = undefined;
    var nd: usize = 0;
    for (a.inodes) |r| if (tables.tableOf(tables.ctx, r.ino) == .local) {
        if (ni == inodes.len) return error.PlanTooLarge;
        inodes[ni] = r;
        ni += 1;
    };
    for (a.dentries) |r| if (tables.tableOf(tables.ctx, r.parent) == .local) {
        if (nd == dents.len) return error.PlanTooLarge;
        dents[nd] = r;
        nd += 1;
    };
    return locks.lockManyUncancelable(io, inodes[0..ni], dents[0..nd]) catch return error.PlanTooLarge;
}

fn askRemote(gpa: Allocator, io: Io, t: *Travelling, tables: Tables, a: AcquireArgs, holder: AppenderId, link: Link) Error!void {
    _ = io;
    var inodes: [dlm.wide_requests]verbs.GuardInodeWire = undefined;
    var ni: usize = 0;
    var dbuf: [dlm.wide_requests * (16 + 256)]u8 = undefined;
    var db: codec.Builder = .init(&dbuf);
    var nd: u16 = 0;
    for (a.inodes) |r| switch (tables.tableOf(tables.ctx, r.ino)) {
        .remote => |x| if (x.holder == holder) {
            inodes[ni] = .{ .ino = r.ino, .exclusive = @intFromBool(r.mode == .exclusive), .reserved = @splat(0) };
            ni += 1;
        },
        else => {},
    };
    for (a.dentries) |r| switch (tables.tableOf(tables.ctx, r.parent)) {
        .remote => |x| if (x.holder == holder) {
            verbs.XvGuardsReq.encodeDentry(&db, .{ .head = .{ .parent = r.parent, .exclusive = @intFromBool(r.mode == .exclusive), .name_len = @intCast(r.name.len), .reserved = @splat(0) }, .name = r.name }) catch return error.PlanTooLarge;
            nd += 1;
        },
        else => {},
    };
    const t0 = clock.monotonicNs();
    stats.xv_cross_owner_guard_rpcs.inc();
    const rep = link.call(link.ctx, .{ .xv_guards = .{ .head = .{ .volume = a.volume, .n_inodes = @intCast(ni), .n_dentries = nd, .reserved = 0, .scope = a.scope }, .inodes = inodes[0..ni], .dentries_bytes = db.written() } }) catch |e| {
        // A refused or timed-out ask gets a fire-and-forget release behind it.
        _ = link.call(link.ctx, .{ .xv_release = .{ .volume = a.volume, .reserved = 0, .reserved2 = 0, .scope = a.scope, .ino = 0 } }) catch {};
        return switch (e) {
            error.StaleHolderView, error.SlotMoved => error.StaleHolderView,
            error.OutOfMemory => error.OutOfMemory,
            else => error.GuardRefused,
        };
    };
    stats.xv_cross_owner_guard_rtt_ns.record(clock.monotonicNs() -| t0);
    _ = rep.xv_guards;
    t.remotes.append(gpa, .{ .holder = holder, .link = link }) catch return error.OutOfMemory;
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the parked scopes: a served XvGuards parks the holder's canonical set under the scope; a second ask under the scope skips covered stripes; the verdict is `covered` by (class, stripe) with the mode floor and `take` otherwise; release drops it; the sweep releases a dead client's scope (the tripwire)" {
    const gpa = testing.allocator;
    var locks = try dlm.Manager.init(gpa, 64);
    defer locks.deinit();
    var parked = ParkedScopes.init(gpa, testing.io, &locks);
    defer parked.deinit();
    const scope = mintScope(5);
    try testing.expect(scope != 0 and scope & (1 << 40) != 0);
    try parked.serve(scope, 7, &.{.{ .ino = 10, .mode = .exclusive }}, &.{.{ .parent = 10, .name = "a", .mode = .exclusive }});
    try testing.expect(parked.isParked(scope));
    try testing.expectEqual(ServeGuards.covered, parked.verdict(scope, &.{.{ .ino = 10, .mode = .shared }}, &.{}));
    try testing.expectEqual(ServeGuards.take, parked.verdict(scope, &.{.{ .ino = 11, .mode = .shared }}, &.{}));
    try testing.expectEqual(ServeGuards.take, parked.verdict(0, &.{}, &.{}));
    // A second ask under the scope: the covered stripe is not re-taken
    // (it would wait behind itself), the new one is.
    try parked.serve(scope, 7, &.{ .{ .ino = 10, .mode = .exclusive }, .{ .ino = 11, .mode = .shared } }, &.{});
    try testing.expectEqual(ServeGuards.covered, parked.verdict(scope, &.{ .{ .ino = 10, .mode = .exclusive }, .{ .ino = 11, .mode = .shared } }, &.{}));
    try testing.expectEqual(ServeGuards.take, parked.verdict(scope, &.{.{ .ino = 11, .mode = .exclusive }}, &.{}));
    try testing.expect(parked.release(scope));
    try testing.expect(!parked.release(scope));
    try testing.expectEqual(@as(usize, 0), parked.count());
    // The stripes are free again.
    var s = (try locks.tryLockMany(testing.io, &.{.{ .ino = 10, .mode = .exclusive }}, &.{})).?;
    s.release(testing.io);
    // The sweep: a live client keeps its scope; a dead one loses it loud.
    const Dead = struct {
        fn isLive(_: *anyopaque, client: u64) bool {
            return client != 9;
        }
    };
    var marker: u8 = 0;
    parked.liveness = .{ .ctx = &marker, .isLive = &Dead.isLive };
    try parked.serve(mintScope(6), 7, &.{.{ .ino = 20, .mode = .exclusive }}, &.{});
    try parked.serve(mintScope(7), 9, &.{.{ .ino = 21, .mode = .exclusive }}, &.{});
    const exp0 = stats.xv_cross_owner_guard_expiries.load();
    stats.xv_cross_owner_guard_expiries.resetLogBudgetForTest();
    @import("../../../core/log.zig").testing.expectErrorLogs(1);
    try testing.expectEqual(@as(u32, 1), parked.sweepExpired(clock.monotonicNs()));
    try @import("../../../core/log.zig").testing.expectAllFired();
    try testing.expectEqual(exp0 + 1, stats.xv_cross_owner_guard_expiries.load());
    try testing.expect(parked.isParked(mintScope(6)) and !parked.isParked(mintScope(7)));
}
